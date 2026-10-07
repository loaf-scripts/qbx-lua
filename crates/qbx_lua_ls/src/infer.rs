use std::borrow::Cow;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use lsp_types::Range;
use qbx_fivem_data::{native, Side};
use qbx_lua_analysis::env::leading_doc_lines;
use qbx_lua_analysis::scope::{LocalId, LocalKind, Resolution, Resolved};
use qbx_lua_analysis::side_guard::SideRegions;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{CommentKind, NumberValue, SmolStr, Span};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::callback_wrappers::{self, Wrapper};
use crate::index::{instance_class, instance_owner, ClassDef, Distance, FileId, Index, Symbol, SymbolKind};
use crate::locate::statement_at;
use crate::luacats::{applies_on, own_type, parse_doc_lines, CastEntry, DocGroup, DocOperator};
use crate::narrow::{Casts, Fact, Flow, Origin, Version, DECLARATION};
use crate::types::{CallbackRole, FunType, Param, Shape, ShapeField, Type, TypeName, TypeParser};

const MAX_DEPTH: u32 = 24;
const MAX_SHAPE_FIELDS: usize = 96;
/// An undocumented function that returns more different sets of values than this keeps only what
/// each position holds across them.
const MAX_RETURN_SETS: usize = 4;
/// How many of the literal values that several assignments store in a field a hover lists, as
/// lua-language-server lists the first few: each entry of a large data table sets one.
const MAX_MERGED_LITERALS: usize = 5;

/// Bits for the kinds of Lua value a type allows, used to pick the `@overload` a call fits.
mod kind {
    pub const NIL: u8 = 1;
    pub const BOOLEAN: u8 = 2;
    pub const NUMBER: u8 = 4;
    pub const STRING: u8 = 8;
    pub const TABLE: u8 = 16;
    pub const FUNCTION: u8 = 32;
    pub const OTHER: u8 = 64;
}

/// How well a call's arguments line up with a signature. Each fit counts the arguments that are one
/// of the literal values their parameter lists, like `"keyPressed"` for `action: "keyPressed"`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fit {
    No,
    /// Only by passing some of them to its `...`.
    ThroughVararg(Literals),
    Exact(Literals),
}

/// The literal arguments a parameter lists, and of those the ones it takes alone: `"keyPressed"`
/// is listed by `"keyPressed"|"keyReleased"|string` too, but only `action: "keyPressed"` pins it.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Literals {
    listed: usize,
    pinned: usize,
}

/// The arguments of one call, each inferred at most once while its signature is chosen and its
/// generics are bound.
struct CallArgs<'e> {
    exprs: &'e [Expr],
    types: Vec<OnceCell<Type>>,
    /// The call is still being written, so the parameters after these arguments may be passed yet.
    open: bool,
}

impl<'e> CallArgs<'e> {
    fn new(exprs: &'e [Expr]) -> Self {
        Self { exprs, types: exprs.iter().map(|_| OnceCell::new()).collect(), open: false }
    }

    /// `exprs` with the types `ty` gives them, leaving the function literals unknown.
    fn typed(exprs: &'e [Expr], ty: impl Fn(&Expr) -> Type) -> Self {
        let typed = |expr: &Expr| match expr.unparen().kind {
            ExprKind::Function(_) => Type::Unknown,
            _ => ty(expr),
        };
        Self { exprs, types: exprs.iter().map(|expr| OnceCell::from(typed(expr))).collect(), open: false }
    }

    fn ty(&self, infer: &Infer, index: usize) -> &Type {
        self.types[index].get_or_init(|| infer.expr(&self.exprs[index]))
    }
}

/// The sets of values a call returns.
type ReturnSets = Rc<Vec<Vec<Type>>>;

/// What a call returns.
#[derive(Default)]
struct Returned {
    values: Vec<Type>,
    /// The sets of values its function returns, when it lists several.
    sets: Vec<Vec<Type>>,
    /// The function declares the values: with `@return`, in a stub or as a native. Those inferred
    /// from the `return`s of a body, or bound from the arguments of a generic, tell what the code
    /// passes today, not what it may.
    declared: bool,
    /// The function returns nothing, so the call gives `nil` whatever its values say.
    nothing: bool,
    /// For values inferred from the `return`s of a body, what those `return`s alone pass, without
    /// the `nil` of a body that runs past its end.
    inferred: Option<Vec<Type>>,
    /// An annotation gives the values, also as the generics bound from the arguments make them.
    annotated: bool,
}

pub enum Decl<'a> {
    Local { stmt: &'a Stmt, index: usize },
    LocalFunction { stmt: &'a Stmt, func: &'a FuncBody },
    Param { func: &'a FuncBody, index: usize, doc_anchor: Option<u32>, expected: Option<Expected<'a>> },
    SelfParam { name: &'a FuncName },
    NumericFor,
    GenericFor { stmt: &'a Stmt, index: usize },
}

/// Where a function literal or table constructor is written, which may declare the type it has to
/// be: the parameters of a function are typed from it, and so are those of the functions a table
/// holds.
#[derive(Clone, Copy)]
pub enum Expected<'a> {
    /// Argument `arg_index` of a call, which has the type of the parameter it is passed for.
    Arg { call: &'a Expr, arg_index: usize },
    /// Value `index` of a `local` statement or an assignment, typed by the `---@type` above it, or
    /// for a function assigned to a field of a class, by the `@field` it implements.
    Value { stmt: &'a Stmt, index: usize },
    /// The function that `function a.b.c()` or `function a.b:c()` defines, typed by the `@field`
    /// `c` of the class `a.b` is.
    Member { stmt: &'a Stmt },
    /// The value of the field `key` of a table constructor, which has the type of that field of the
    /// table.
    Field { table: &'a Expr, key: FieldKey<'a> },
    /// Value `index` of a `return` of the function `func`, whose doc comment is at `doc_anchor`,
    /// typed by the `@return` there or by the function type something declares `func` has.
    Return { func: &'a FuncBody, doc_anchor: Option<u32>, index: usize },
}

/// How a table constructor keys one of its fields.
#[derive(Clone, Copy)]
pub enum FieldKey<'a> {
    /// `name = value` or `['name'] = value`.
    Name(&'a str),
    /// The `n`th value written without a key.
    Position(i64),
    /// `[key] = value` with a key that is no string.
    Expr(&'a Expr),
}

pub struct FileContext<'a> {
    pub file: FileId,
    pub source: &'a str,
    pub chunk: &'a Chunk,
    pub resolution: &'a Resolution,
    decls: FxHashMap<u32, Decl<'a>>,
    /// The start of the statement each call is the value of, by the start of the call.
    call_anchors: FxHashMap<u32, u32>,
    /// Where each table constructor that a declared type may describe is written, by its start.
    tables: FxHashMap<u32, Expected<'a>>,
    /// Where each function literal that a declared type may describe is written, by the start of
    /// its parameter list.
    functions: FxHashMap<u32, Expected<'a>>,
    /// The `table.__index = value` assignments of the file, by the table and the value.
    index_writes: Vec<(&'a Expr, &'a Expr)>,
    /// The metatables that `setmetatable(name, metatable)` calls give each local, with where each
    /// call ends.
    set_metatables: FxHashMap<LocalId, Vec<(u32, &'a Expr)>>,
    docs: RefCell<FxHashMap<u32, Rc<DocGroup>>>,
    flow: OnceCell<Flow<'a>>,
    casts: OnceCell<Casts>,
}

impl<'a> FileContext<'a> {
    pub fn new(file: FileId, source: &'a str, chunk: &'a Chunk, resolution: &'a Resolution) -> Self {
        let mut collector = DeclCollector {
            source,
            decls: FxHashMap::default(),
            call_anchors: FxHashMap::default(),
            tables: FxHashMap::default(),
            functions: FxHashMap::default(),
            index_writes: Vec::new(),
            set_metatables: Vec::new(),
            enclosing: Vec::new(),
        };
        collector.block(&chunk.block);
        let mut set_metatables: FxHashMap<LocalId, Vec<(u32, &'a Expr)>> = FxHashMap::default();
        for (name, call_end, metatable) in collector.set_metatables {
            if let Some(Resolved::Local(id)) = resolution.resolve_at(name.span.start) {
                set_metatables.entry(id).or_default().push((call_end, metatable));
            }
        }
        Self {
            file,
            source,
            chunk,
            resolution,
            decls: collector.decls,
            call_anchors: collector.call_anchors,
            tables: collector.tables,
            functions: collector.functions,
            index_writes: collector.index_writes,
            set_metatables,
            docs: RefCell::new(FxHashMap::default()),
            flow: OnceCell::new(),
            casts: OnceCell::new(),
        }
    }

    /// Which values the locals of the file may hold at each point, and what the conditions tell about
    /// them.
    pub fn flow(&self) -> &Flow<'a> {
        self.flow.get_or_init(|| Flow::of(self.chunk, self.resolution, self.casts()))
    }

    /// The `---@cast` lines of the file.
    pub fn casts(&self) -> &Casts {
        self.casts.get_or_init(|| Casts::of(self.source, self.chunk, self.resolution))
    }

    pub fn decl(&self, decl_start: u32) -> Option<&Decl<'a>> {
        self.decls.get(&decl_start)
    }

    /// Whether a `setmetatable(name, metatable)` call gives the local `id` a metatable.
    pub fn sets_metatable(&self, id: LocalId) -> bool {
        self.set_metatables.contains_key(&id)
    }

    pub fn doc_at(&self, stmt_start: u32) -> Rc<DocGroup> {
        if let Some(doc) = self.docs.borrow().get(&stmt_start) {
            return doc.clone();
        }
        let lines = leading_doc_lines(self.source, &self.chunk.comments, stmt_start);
        let doc = Rc::new(parse_doc_lines(&lines));
        self.docs.borrow_mut().insert(stmt_start, doc.clone());
        doc
    }

    /// The functions `stmt` defines, grouped by the doc comment they take: those of
    /// `documented_functions`, under the comment above the statement, and each function its values
    /// write in a field of a table constructor, under the comment above that field.
    pub fn function_docs<'s>(&self, stmt: &'s Stmt) -> Vec<(Rc<DocGroup>, Vec<&'s FuncBody>)> {
        let mut out = Vec::new();
        let functions = documented_functions(stmt);
        if !functions.is_empty() {
            out.push((self.doc_at(stmt.span.start), functions));
        }
        let values: &[Expr] = match &stmt.kind {
            StmtKind::Local { exprs, .. } | StmtKind::Assign { exprs, .. } | StmtKind::Return(exprs) => exprs,
            StmtKind::Expr(expr) => std::slice::from_ref(expr),
            _ => &[],
        };
        for value in values {
            self.field_functions(value, &mut out);
        }
        out
    }

    /// Adds each function written in a field of a table constructor in `expr`, outside other
    /// functions, with the doc comment above its field.
    fn field_functions<'s>(&self, expr: &'s Expr, out: &mut Vec<(Rc<DocGroup>, Vec<&'s FuncBody>)>) {
        match &expr.kind {
            ExprKind::Table(fields) => {
                for field in fields {
                    let value = match field {
                        TableField::Named { value, .. } | TableField::Positional(value) => value,
                        TableField::Keyed { key, value } => {
                            self.field_functions(key, out);
                            value
                        }
                        TableField::SetMember(_) => continue,
                    };
                    match &value.kind {
                        ExprKind::Function(func) => {
                            out.push((self.doc_at(field_start(self.source, field)), vec![&**func]))
                        }
                        _ => self.field_functions(value, out),
                    }
                }
            }
            ExprKind::Call { callee: base, args, .. } | ExprKind::MethodCall { base, args, .. } => {
                self.field_functions(base, out);
                args.iter().for_each(|arg| self.field_functions(arg, out));
            }
            ExprKind::Index { base, index: other, .. } | ExprKind::Binary { lhs: base, rhs: other, .. } => {
                self.field_functions(base, out);
                self.field_functions(other, out);
            }
            ExprKind::Field { base: inner, .. } | ExprKind::Unary { expr: inner, .. } | ExprKind::Paren(inner) => {
                self.field_functions(inner, out)
            }
            _ => {}
        }
    }

    /// Where the doc comment of the functions passed to a call is read: the start of the call, or
    /// of the `x = f(...)` or `local x = f(...)` statement the call is the value of.
    pub fn call_doc_anchor(&self, call_start: u32) -> u32 {
        self.call_anchors.get(&call_start).copied().unwrap_or(call_start)
    }

    pub fn local_owner_key(&self, decl_start: u32) -> SmolStr {
        SmolStr::new(format!("%f{}:{}", self.file, decl_start))
    }

    /// The declaration behind a `local_owner_key` of this file.
    pub fn local_owner_decl(&self, owner: &str) -> Option<u32> {
        let (file, decl_start) = owner.strip_prefix("%f")?.split_once(':')?;
        (file.parse::<FileId>().ok()? == self.file).then(|| decl_start.parse().ok()).flatten()
    }
}

struct DeclCollector<'a> {
    source: &'a str,
    decls: FxHashMap<u32, Decl<'a>>,
    call_anchors: FxHashMap<u32, u32>,
    tables: FxHashMap<u32, Expected<'a>>,
    functions: FxHashMap<u32, Expected<'a>>,
    index_writes: Vec<(&'a Expr, &'a Expr)>,
    set_metatables: Vec<(&'a Name, u32, &'a Expr)>,
    /// The functions whose bodies are being walked, innermost last, with their doc comments.
    enclosing: Vec<(&'a FuncBody, Option<u32>)>,
}

impl<'a> DeclCollector<'a> {
    fn block(&mut self, block: &'a Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    fn func(&mut self, func: &'a FuncBody, doc_anchor: Option<u32>, expected: Option<Expected<'a>>) {
        if let Some(expected) = expected {
            self.functions.insert(func.params_span.start, expected);
        }
        for (index, param) in func.params.iter().enumerate() {
            self.decls.insert(param.span.start, Decl::Param { func, index, doc_anchor, expected });
        }
        self.enclosing.push((func, doc_anchor));
        self.block(&func.body);
        self.enclosing.pop();
    }

    fn stmt(&mut self, stmt: &'a Stmt) {
        let anchor = Some(stmt.span.start);
        match &stmt.kind {
            StmtKind::Local { names, exprs, in_unpack } => {
                for (index, name) in names.iter().enumerate() {
                    self.decls.insert(name.name.span.start, Decl::Local { stmt, index });
                }
                for (index, expr) in exprs.iter().enumerate() {
                    // `local a, b in t` takes the names from `t`, which the `---@type` above does not describe.
                    if *in_unpack {
                        self.expr(expr, anchor);
                    } else {
                        self.value(expr, anchor, Expected::Value { stmt, index });
                    }
                }
            }
            StmtKind::LocalFunction { name, func } => {
                self.decls.insert(name.span.start, Decl::LocalFunction { stmt, func });
                self.func(func, anchor, None);
            }
            StmtKind::Function { name, func } => {
                if name.method.is_some() {
                    self.decls.insert(func.params_span.start, Decl::SelfParam { name });
                }
                self.func(func, anchor, Some(Expected::Member { stmt }));
            }
            StmtKind::Assign { targets, exprs } => {
                targets.iter().for_each(|e| self.expr(e, None));
                for (target, value) in targets.iter().zip(exprs) {
                    match &target.kind {
                        ExprKind::Field { base, name, .. } if name.text == "__index" => {
                            self.index_writes.push((base, value));
                        }
                        ExprKind::Index { base, index, .. }
                            if index.as_string().is_some_and(|key| key == "__index") =>
                        {
                            self.index_writes.push((base, value));
                        }
                        _ => {}
                    }
                }
                for (index, expr) in exprs.iter().enumerate() {
                    self.value(expr, anchor, Expected::Value { stmt, index });
                }
            }
            StmtKind::CompoundAssign { target, expr, .. } => {
                self.expr(target, None);
                self.expr(expr, None);
            }
            StmtKind::Expr(expr) => self.expr(expr, anchor),
            StmtKind::Do(body) | StmtKind::Defer(body) => self.block(body),
            StmtKind::While { cond, body } => {
                self.expr(cond, None);
                self.block(body);
            }
            StmtKind::Repeat { body, cond } => {
                self.block(body);
                self.expr(cond, None);
            }
            StmtKind::If { branches, else_block } => {
                for branch in branches {
                    self.expr(&branch.cond, None);
                    self.block(&branch.block);
                }
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            StmtKind::NumericFor { var, start, limit, step, body } => {
                self.decls.insert(var.span.start, Decl::NumericFor);
                self.expr(start, None);
                self.expr(limit, None);
                if let Some(step) = step {
                    self.expr(step, None);
                }
                self.block(body);
            }
            StmtKind::GenericFor { names, exprs, body } => {
                for (index, name) in names.iter().enumerate() {
                    self.decls.insert(name.span.start, Decl::GenericFor { stmt, index });
                }
                exprs.iter().for_each(|e| self.expr(e, None));
                self.block(body);
            }
            StmtKind::Return(exprs) => {
                let enclosing = self.enclosing.last().copied();
                for (index, expr) in exprs.iter().enumerate() {
                    match (&expr.kind, enclosing) {
                        // `return function(ped) end` is the `fun(ped: number)` that the `@return` of its
                        // function declares, and takes the `@param` lines above the `return`.
                        (ExprKind::Function(func), Some((outer, doc_anchor))) => {
                            let at = Expected::Return { func: outer, doc_anchor, index };
                            self.func(func, anchor, Some(at));
                        }
                        _ => self.expr(expr, None),
                    }
                }
            }
            StmtKind::Break | StmtKind::Goto(_) | StmtKind::Label(_) | StmtKind::Error => {}
        }
    }

    fn expr(&mut self, expr: &'a Expr, doc_anchor: Option<u32>) {
        match &expr.kind {
            ExprKind::Function(func) => self.func(func, doc_anchor, None),
            ExprKind::Call { callee, args, .. } => {
                if let (ExprKind::Name(callee), [Expr { kind: ExprKind::Name(name), .. }, metatable, ..]) =
                    (&callee.kind, &args[..])
                {
                    if callee.text == "setmetatable" {
                        self.set_metatables.push((name, expr.span.end, metatable));
                    }
                }
                self.expr(callee, None);
                self.call_args(expr, args, doc_anchor);
            }
            ExprKind::MethodCall { base, args, .. } => {
                self.expr(base, None);
                self.call_args(expr, args, doc_anchor);
            }
            ExprKind::Index { base, index, .. } => {
                self.expr(base, None);
                self.expr(index, None);
            }
            ExprKind::Field { base, .. } => self.expr(base, None),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs, None);
                self.expr(rhs, None);
            }
            ExprKind::Unary { expr, .. } | ExprKind::Paren(expr) => self.expr(expr, None),
            ExprKind::Table(fields) => {
                let mut position = 0;
                for field in fields {
                    let (key, value) = match field {
                        TableField::Named { name, value } => (FieldKey::Name(&name.text), value),
                        TableField::Keyed { key, value } => {
                            self.expr(key, None);
                            (key.as_string().map_or(FieldKey::Expr(key), |name| FieldKey::Name(name)), value)
                        }
                        TableField::Positional(value) => {
                            position += 1;
                            (FieldKey::Position(position), value)
                        }
                        TableField::SetMember(_) => continue,
                    };
                    let at = Expected::Field { table: expr, key };
                    // A function in a field takes the `@param` lines above the field.
                    match &value.kind {
                        ExprKind::Function(func) => self.func(func, Some(field_start(self.source, field)), Some(at)),
                        _ => self.value(value, None, at),
                    }
                }
            }
            _ => {}
        }
    }

    /// A value written where `at` may declare its type. A function takes the doc comment at
    /// `doc_anchor`, and a call passes it on to the functions passed to it.
    fn value(&mut self, expr: &'a Expr, doc_anchor: Option<u32>, at: Expected<'a>) {
        match &expr.kind {
            ExprKind::Function(func) => self.func(func, doc_anchor, Some(at)),
            ExprKind::Table(_) => {
                self.tables.insert(expr.span.start, at);
                self.expr(expr, None);
            }
            // `X = X or function(appearance) end` gives the function what the statement declares, as
            // TypeScript types both sides of `||` and the right side of `&&`.
            ExprKind::Binary { op: BinOp::Or | BinOp::And, .. } if alternative_function(expr).is_some() => {
                self.alternatives(expr, doc_anchor, at)
            }
            _ => self.expr(expr, doc_anchor),
        }
    }

    /// The sides of `a or b` and `a and b` that may be the value of the expression, with the
    /// function among them taking what `at` declares. A table there keeps the type it is built with,
    /// as `Config = Config or {}` is filled in later.
    fn alternatives(&mut self, expr: &'a Expr, doc_anchor: Option<u32>, at: Expected<'a>) {
        match &expr.kind {
            ExprKind::Function(func) => self.func(func, doc_anchor, Some(at)),
            ExprKind::Paren(inner) => self.alternatives(inner, doc_anchor, at),
            ExprKind::Binary { op: BinOp::Or, lhs, rhs, .. } => {
                self.alternatives(lhs, doc_anchor, at);
                self.alternatives(rhs, doc_anchor, at);
            }
            ExprKind::Binary { op: BinOp::And, lhs, rhs, .. } => {
                self.expr(lhs, None);
                self.alternatives(rhs, doc_anchor, at);
            }
            _ => self.expr(expr, None),
        }
    }

    /// `doc_anchor` is the statement the call is the value of, whose doc comment the functions
    /// passed to the call take. Without one, they take the doc comment above the call.
    fn call_args(&mut self, call: &'a Expr, args: &'a [Expr], doc_anchor: Option<u32>) {
        if let Some(anchor) = doc_anchor {
            self.call_anchors.insert(call.span.start, anchor);
        }
        for (arg_index, arg) in args.iter().enumerate() {
            let at = Expected::Arg { call, arg_index };
            match &arg.kind {
                ExprKind::Function(func) => self.func(func, Some(doc_anchor.unwrap_or(call.span.start)), Some(at)),
                _ => self.value(arg, None, at),
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct MemberInfo {
    pub name: SmolStr,
    pub ty: Type,
    pub doc: Option<Arc<str>>,
    pub deprecated: bool,
    pub literal: Option<SmolStr>,
    pub kind: SymbolKind,
    pub location: Option<(FileId, Range)>,
    /// The type is inferred from the value that an assignment without `---@type` stores, so other
    /// assignments to the same field may store values of other types.
    pub inferred: bool,
}

pub struct Infer<'a> {
    pub ctx: &'a FileContext<'a>,
    pub index: &'a Index,
    /// The manifest side of the file, which decides the `(server)` and `(client)` classes, aliases
    /// and fields it sees.
    side: Option<Side>,
    /// Guarded regions, which narrow the side of the calls in them for `(server)` overloads.
    regions: OnceCell<SideRegions>,
    locals: RefCell<FxHashMap<LocalId, Type>>,
    in_progress: RefCell<FxHashSet<LocalId>>,
    /// The types of the values that the declarations and assignments of locals give them, by the
    /// local and the origin of the value.
    origins: RefCell<FxHashMap<(LocalId, u32), Type>>,
    origins_in_progress: RefCell<FxHashSet<(LocalId, u32)>>,
    /// The types of the values that each set of `Origin::Others` of a local stands for, each once,
    /// by the local and the origin of the set.
    others: RefCell<SetTypes>,
    /// The types of the values that assignments store in the fields of locals, by their origin.
    stored: RefCell<FxHashMap<u32, Type>>,
    storing: RefCell<FxHashSet<u32>>,
    /// The sets of values returned by the call of each `local a, b = f()` statement read so far,
    /// by the start of the statement.
    linked_sets: RefCell<FxHashMap<u32, Option<ReturnSets>>>,
    /// The tables that each owner falls back on through the `__index` of its metatables, by owner.
    fallbacks: RefCell<FxHashMap<SmolStr, Type>>,
    /// The owners whose fallbacks are being looked up, which a metatable chain that loops back stops at.
    following: RefCell<FxHashSet<SmolStr>>,
    /// What the `__index` of the metatable passed to each `setmetatable` call gives, by the start of
    /// that argument.
    passed_indexes: RefCell<FxHashMap<u32, Type>>,
    depth: Cell<u32>,
    /// Set when `guarded` cuts a lookup short at `MAX_DEPTH`, so that what it found is not cached.
    truncated: Cell<bool>,
    /// The functions of this file whose types are inferred for the index, by the name that declares
    /// each and its range, as `typing` sets them.
    typing: RefCell<Vec<(SmolStr, Range)>>,
    /// Whether the diagnostics this inference serves follow `strict`; see `Infer::strict`.
    strict: bool,
}

/// Natives call their handles `Vehicle`, `Ped` and so on. They are integers, and resources (ox_lib,
/// qbx_core) declare unrelated classes under the same names, so they must not resolve as classes.
pub(crate) const NATIVE_HANDLE_TYPES: &[&str] =
    &["Vehicle", "Ped", "Entity", "Object", "Player", "Hash", "Cam", "Blip", "Pickup", "ScrHandle", "FireId"];

fn native_type(name: &str) -> Type {
    if NATIVE_HANDLE_TYPES.contains(&name) {
        return Type::Handle(SmolStr::new(name));
    }
    // The callbacks that natives take are written as LuaCATS types, as `fun(source: integer, ...)`,
    // and so are the tables some return, as `Player[]`.
    if name.starts_with("fun(") {
        return TypeParser::new(name).parse();
    }
    if name.contains(['[', '{']) {
        return with_handles(TypeParser::new(name).parse());
    }
    Type::named(name)
}

/// `ty` with the handle names in its arrays and tuples read as handles, as `native_type` reads one
/// alone: the `Player` of `Player[]`.
fn with_handles(ty: Type) -> Type {
    match ty {
        Type::Named(name, args) if args.is_empty() && NATIVE_HANDLE_TYPES.contains(&name.text.as_str()) => {
            Type::Handle(name.text)
        }
        Type::Array(inner) => Type::Array(Box::new(with_handles(*inner))),
        Type::Tuple(items) => Type::Tuple(items.into_iter().map(with_handles).collect()),
        other => other,
    }
}

pub fn native_fun_type(native: &qbx_fivem_data::Native) -> FunType {
    FunType {
        params: native
            .params()
            .map(|(name, ty, optional)| Param {
                name: SmolStr::new(name),
                ty: native_type(ty),
                optional,
                ..Param::default()
            })
            .collect(),
        returns: native.returns().map(native_type).collect(),
        ..FunType::default()
    }
}

impl<'a> Infer<'a> {
    pub fn new(ctx: &'a FileContext<'a>, index: &'a Index) -> Self {
        Self::with_side(ctx, index, index.file(ctx.file).and_then(|f| f.side))
    }

    pub fn with_side(ctx: &'a FileContext<'a>, index: &'a Index, side: Option<Side>) -> Self {
        Self {
            ctx,
            index,
            side,
            regions: OnceCell::new(),
            locals: RefCell::new(FxHashMap::default()),
            in_progress: RefCell::new(FxHashSet::default()),
            origins: RefCell::new(FxHashMap::default()),
            origins_in_progress: RefCell::new(FxHashSet::default()),
            others: RefCell::new(FxHashMap::default()),
            stored: RefCell::new(FxHashMap::default()),
            storing: RefCell::new(FxHashSet::default()),
            linked_sets: RefCell::new(FxHashMap::default()),
            fallbacks: RefCell::new(FxHashMap::default()),
            following: RefCell::new(FxHashSet::default()),
            passed_indexes: RefCell::new(FxHashMap::default()),
            depth: Cell::new(0),
            truncated: Cell::new(false),
            typing: RefCell::new(Vec::new()),
            strict: false,
        }
    }

    /// An inference for the diagnostics of a file whose configuration sets `strict` so.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Whether the diagnostics of the file follow the `strict` setting, reporting what TypeScript's
    /// strict mode reports where lua-language-server does not, such as `n + 1` for a `number?`.
    /// Rules that check something only in strict mode, or less of it outside, ask here. Only the
    /// diagnostics set it: what the index, hover and completion infer never depends on it.
    pub fn strict(&self) -> bool {
        self.strict
    }

    /// Runs `infer` while the function that `name` at `range` declares in this file is the one
    /// whose type is inferred. The index still holds what it returned before, so a call of it within
    /// itself reads as unknown, as lua-language-server reads it, rather than as a type that grows
    /// each time the file is indexed again.
    pub fn typing<T>(&self, name: &SmolStr, range: Range, infer: impl FnOnce() -> T) -> T {
        self.typing.borrow_mut().push((name.clone(), range));
        let out = infer();
        self.typing.borrow_mut().pop();
        out
    }

    /// Whether calling `base`, or its `method`, calls a function whose type `typing` infers.
    fn calls_itself(&self, base: &Expr, method: Option<&Name>) -> bool {
        let (owner, name) = match (method, &base.kind) {
            (Some(method), _) => (Some(base), &method.text),
            (None, ExprKind::Field { base: owner, name, .. }) => (Some(&**owner), &name.text),
            (None, ExprKind::Name(name)) => (None, &name.text),
            _ => return false,
        };
        let typing = self.typing.borrow();
        let typed: Vec<Range> = typing.iter().filter(|(typed, _)| typed == name).map(|(_, range)| *range).collect();
        drop(typing);
        if typed.is_empty() {
            return false;
        }
        let declared = |file: FileId, range: Range| file == self.ctx.file && typed.contains(&range);
        match owner {
            Some(owner) => {
                let members = self.members_named(&self.expr(owner), name);
                members.iter().any(|member| member.location.is_some_and(|(file, range)| declared(file, range)))
            }
            None => {
                let global = !matches!(self.ctx.resolution.resolve_at(base.span.start), Some(Resolved::Local(_)));
                let symbols = self.index.globals_named(name, self.ctx.file);
                global && symbols.iter().any(|(file, symbol)| declared(*file, symbol.range))
            }
        }
    }

    /// The manifest side of the file.
    pub fn side(&self) -> Option<Side> {
        self.side
    }

    /// The side of the code at `offset`, narrowed by `IsDuplicityVersion()` and `lib.context` guards.
    pub fn side_at(&self, offset: u32) -> Option<Side> {
        self.regions.get_or_init(|| SideRegions::of(self.ctx.source, self.ctx.chunk)).effective(offset, self.side)
    }

    fn guarded<T: Default>(&self, f: impl FnOnce() -> T) -> T {
        if self.depth.get() >= MAX_DEPTH {
            self.truncated.set(true);
            return T::default();
        }
        self.depth.set(self.depth.get() + 1);
        let out = f();
        self.depth.set(self.depth.get() - 1);
        out
    }

    /// What `f` gives, and whether it is complete: a lookup that `MAX_DEPTH` cut short may find
    /// more when it starts from a shallower point, so it is not cached.
    fn complete<T>(&self, f: impl FnOnce() -> T) -> (T, bool) {
        let outer = self.truncated.replace(false);
        let out = f();
        let truncated = self.truncated.get();
        self.truncated.set(outer || truncated);
        (out, !truncated)
    }

    pub fn expr(&self, expr: &Expr) -> Type {
        self.expr_multi(expr).into_iter().next().unwrap_or_default()
    }

    /// The one value that `expr` gives a name or a field, as `expr` gives it, but a `...T` that a
    /// call returns gives a `T`.
    fn first_value(&self, expr: &Expr) -> Type {
        value_at(self.expr_multi(expr), 0).unwrap_or_default()
    }

    pub fn expr_multi(&self, expr: &Expr) -> Vec<Type> {
        let mut values = self.guarded(|| match &expr.kind {
            ExprKind::Call { callee, args, .. } => self.call(callee, None, args),
            ExprKind::MethodCall { base, method, args, .. } => self.call(base, Some(method), args),
            _ => vec![self.single(expr)],
        });
        if let Some(cast) = self.cast_after(expr.span.end) {
            match values.first_mut() {
                Some(first) => *first = cast,
                None => values.push(cast),
            }
        }
        values
    }

    /// The type that a `--[[@as T]]` or `---@as T` comment casts the expression ending at `end` to,
    /// when nothing but spaces stands between them.
    pub fn cast_after(&self, end: u32) -> Option<Type> {
        let comments = &self.ctx.chunk.comments;
        let comment = comments.get(comments.partition_point(|c| c.span.start < end))?;
        let between = self.ctx.source.get(end as usize..comment.span.start as usize)?;
        if !between.bytes().all(|b| b == b' ' || b == b'\t') {
            return None;
        }
        let content = comment.content.text(self.ctx.source);
        let content = match comment.kind {
            CommentKind::Long => content,
            CommentKind::Line => content.strip_prefix('-')?,
            CommentKind::CStyle => return None,
        };
        let ty = content.trim_start().strip_prefix("@as")?;
        ty.starts_with(char::is_whitespace).then(|| TypeParser::new(ty.trim()).parse())
    }

    fn single(&self, expr: &Expr) -> Type {
        match &expr.kind {
            ExprKind::Nil => Type::Nil,
            ExprKind::True => Type::BooleanLit(true),
            ExprKind::False => Type::BooleanLit(false),
            ExprKind::Number(NumberValue::Int(i)) => Type::IntLit(*i),
            ExprKind::Number(NumberValue::Float(_)) => Type::Number,
            ExprKind::String(s) => Type::StringLit(s.clone()),
            ExprKind::JenkinsHash(_) => Type::Integer,
            ExprKind::Vararg => Type::Any,
            ExprKind::Function(func) => Type::Fun(Arc::new(self.fun_type(func, None, false))),
            ExprKind::Name(name) => self.name(name),
            ExprKind::Paren(inner) => self.expr(inner),
            ExprKind::Field { base, name, .. } => self.field_narrowed(expr, self.member_type(base, name)),
            ExprKind::Index { base, index, .. } => self.field_narrowed(expr, self.index_expr(base, index)),
            ExprKind::Table(fields) => self.table(fields),
            ExprKind::Binary { op, lhs, rhs, .. } => self.binary(*op, lhs, rhs),
            ExprKind::Unary { op, expr } => self.unary(*op, expr),
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::Error => Type::Unknown,
        }
    }

    /// The type of the field `name` of `base`, as it is declared or assigned.
    fn member_type(&self, base: &Expr, name: &Name) -> Type {
        if let ExprKind::Name(root) = &base.kind {
            let is_env = matches!(root.text.as_str(), "_ENV" | "_G")
                && matches!(self.ctx.resolution.resolve_at(root.span.start), Some(Resolved::Global(_)));
            if is_env {
                return self.global_type(&name.text);
            }
        }
        let base_ty = self.expr(base);
        self.member(&base_ty, &name.text).map(|m| m.ty).unwrap_or_default()
    }

    /// The type of what an assignment to `target` stores: what its local is declared with, whatever
    /// it holds before, or the type of the field it sets, which no guard narrows.
    pub fn target_type(&self, target: &Expr) -> Type {
        match &target.kind {
            ExprKind::Name(name) => match self.ctx.resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => self.local_type(id),
                _ => self.global_type(&name.text),
            },
            ExprKind::Field { base, name, .. } => self.member_type(base, name),
            ExprKind::Index { base, index, .. } => self.index_expr(base, index),
            _ => self.expr(target),
        }
    }

    fn name(&self, name: &Name) -> Type {
        match self.ctx.resolution.resolve_at(name.span.start) {
            Some(Resolved::Local(id)) => self.local_type_at(id, name.span.start),
            _ => self.global_type(&name.text),
        }
    }

    /// The type of a local where it is read at `offset`: the types of the values that may reach the
    /// read, as the `---@cast` lines before the read change them, without what the guards on the way
    /// rule out, such as the `nil` of a `string?` after `if not name then return end`. Where an
    /// assignment names the local, the type of the value it gives.
    pub fn local_type_at(&self, id: LocalId, offset: u32) -> Type {
        let flow = self.ctx.flow();
        if let Some(origin) = flow.written_at(offset) {
            // `name += 1` reads the value it replaces there, and a value of no known type tells less
            // than the type of the local.
            let given = self.origin_type(id, origin).unwrap_or_default();
            if !matches!(flow.origin(origin), Origin::Compound { .. }) && !given.is_unknown() {
                return self.with_metatables(id, offset, given);
            }
        }
        let origin = |origin| self.origin_bases(id, origin).map(|bases| self.with_metatables_all(id, offset, bases));
        self.narrowed_with(id, offset, &origin, false)
    }

    /// The type of a local where it is read at `offset`, as `local_type_at` gives it, or `unknown`
    /// when a value of unknown type may reach the read.
    pub fn known_local_type_at(&self, id: LocalId, offset: u32) -> Type {
        let origin = |origin| self.origin_bases(id, origin).map(|bases| self.with_metatables_all(id, offset, bases));
        self.narrowed_with(id, offset, &origin, true)
    }

    /// Whether an assignment may give the local `id` a value of unknown type that reaches its read at
    /// `offset`. `origin_type` gives such a value the type the local is declared with, as the type of
    /// `local name = ''` stays a `string` in TypeScript, while lua-language-server reads the local as
    /// unknown there.
    pub fn may_hold_unknown(&self, id: LocalId, offset: u32) -> bool {
        let flow = self.ctx.flow();
        let unknown = |origin: u32| {
            let Origin::Assignment { stmt, index } = flow.origin(origin) else { return false };
            let StmtKind::Assign { exprs, .. } = &stmt.kind else { return false };
            self.ctx.doc_at(stmt.span.start).type_at(index).is_none()
                && assigned_value(exprs, index, |expr| self.expr(expr), |expr| self.expr_multi(expr)).is_unknown()
        };
        flow.at(id, offset).iter().any(|value| match flow.origin(value.origin) {
            Origin::Others(set) => flow.others(set).iter().any(|origin| unknown(*origin)),
            _ => unknown(value.origin),
        })
    }

    /// `bases`, the types of values of the local `id`, with what the `setmetatable` calls on the local
    /// make of them where it is read at `offset`.
    fn with_metatables_all(&self, id: LocalId, offset: u32, bases: Bases) -> Bases {
        if !self.ctx.set_metatables.contains_key(&id) {
            return bases;
        }
        match bases {
            Bases::One(ty) => Bases::One(self.with_metatables(id, offset, ty)),
            Bases::Set(types) => {
                Bases::Set(types.iter().map(|ty| self.with_metatables(id, offset, ty.clone())).collect())
            }
        }
    }

    /// `ty`, the type of the local `id`, with what the `setmetatable` calls on the local make of it
    /// where it is read at `offset`: `setmetatable(obj, Class)` makes `obj` an instance of `Class`.
    /// For a table class that holds before the call too, so the fields a constructor sets on `obj`
    /// first are those of its instances. A `---@class` is a type that `obj` takes only once the
    /// call gives it, so what is set on `obj` before stays out of the class.
    fn with_metatables(&self, id: LocalId, offset: u32, ty: Type) -> Type {
        let Some(metatables) = self.ctx.set_metatables.get(&id) else { return ty };
        // The index gives a table of its own the metatables set on it.
        if self.is_own_table(id, &ty) {
            return ty;
        }
        metatables.iter().fold(ty, |ty, (call_end, metatable)| {
            let index = self.passed_index(metatable);
            if offset < *call_end && matches!(index, Type::Named(..)) {
                return ty;
            }
            self.instance_type(ty, index, false)
        })
    }

    /// Whether `ty`, the type of the local `id`, is the table the local is declared with, as the
    /// top-level `local Base = {}` has, rather than one it holds, as an instance has the type of its
    /// class.
    fn is_own_table(&self, id: LocalId, ty: &Type) -> bool {
        let own = self.ctx.local_owner_key(self.ctx.resolution.local(id).decl.start);
        matches!(ty.without_nil(), Type::GlobalTable(owner) if owner == own)
    }

    /// Whether `expr`, of type `ty`, names a table of its own, which the index gives the metatables
    /// that `setmetatable` sets on it: a top-level local declared with a table, or a global or a
    /// field of one that holds its own table rather than an instance of another.
    fn names_own_table(&self, expr: &Expr, ty: &Type) -> bool {
        if let ExprKind::Name(name) = &expr.kind {
            if let Some(Resolved::Local(id)) = self.ctx.resolution.resolve_at(name.span.start) {
                return self.is_own_table(id, ty);
            }
        }
        let path = expr.dotted_path();
        matches!(ty.without_nil(), Type::GlobalTable(owner) if path.is_some_and(|path| owner == path))
    }

    /// The type of the local `id` where it is read at `offset`: the union of the values that may
    /// reach the read, each of the type `origin` gives for where it comes from (`None` while that is
    /// being found, as for a loop that assigns the local from what it held before), its declaration,
    /// an assignment or a `---@cast` line, without what the guards since then rule out. A value that the
    /// guards rule out entirely is left out. When they rule out every value, the code they guard
    /// handles values the annotations leave out, or never runs, and it takes the local to be what
    /// `Fact::ruled_out` reads from the guard that rules out the last of them, as lua-language-server
    /// does: `nil` inside `if not name` for a `string`, `unknown` inside `if type(list) ~= 'table'`
    /// for a `number[]`. After a comparison with a literal, as `action ~= "open" and action ~= "close"`
    /// for an `"open"|"close"`, it stays whole. With `strict`, a value of unknown type leaves the
    /// local unknown.
    pub fn narrowed_with(&self, id: LocalId, offset: u32, origin: &dyn Fn(u32) -> Option<Bases>, strict: bool) -> Type {
        let values = self.ctx.flow().at(id, offset);
        // `local name` declares no value that the values assigned later have to make room for.
        let unset = values.len() > 1 && self.is_unset(id);
        let (mut whole, mut kept, mut taken) = (Vec::new(), Vec::new(), Some(Vec::new()));
        for value in values.iter().filter(|value| !(unset && value.origin == DECLARATION)) {
            let Some(bases) = origin(value.origin) else { continue };
            for base in bases.iter() {
                if strict && base.is_unknown() {
                    return Type::Unknown;
                }
                let (ty, left) = self.version_type(id, offset, value, base.clone());
                match left {
                    Ok(left) => kept.push(left),
                    Err(Some(as_is)) => taken.iter_mut().for_each(|taken| taken.push(as_is.clone())),
                    Err(None) => taken = None,
                }
                whole.push(ty);
            }
        }
        let types = match (kept.is_empty(), taken) {
            (false, _) => kept,
            (true, Some(taken)) if !taken.is_empty() => taken,
            _ => whole,
        };
        merge_versions(types)
    }

    /// What `value` of the local `id` is where it is read at `offset`: `base`, the type of where it
    /// comes from, and what `facts_left` leaves of that after the guards since.
    fn version_type(
        &self,
        id: LocalId,
        offset: u32,
        value: &Version,
        base: Type,
    ) -> (Type, Result<Type, Option<Type>>) {
        let mut ty = base;
        if value.origin == DECLARATION {
            ty = self.linked_type(id, Span::empty(offset)).unwrap_or(ty);
        }
        let left = self.facts_left(&ty, value.facts.iter().map(|(_, fact)| fact));
        (ty, left)
    }

    /// `ty` without what `facts` rule out, or when they rule out every value of it, the type that the
    /// code they guard takes the value to be then, as `Fact::ruled_out` reads it from the guard that
    /// rules out the last of them, if it gives one. An alias such as `Name = string|nil` is narrowed
    /// through what it stands for, and a type the facts leave whole stays as it is written.
    fn facts_left<'f>(&self, ty: &Type, facts: impl Iterator<Item = &'f Fact>) -> Result<Type, Option<Type>> {
        let mut facts = facts.flat_map(|fact| self.typed_facts(fact)).flatten().peekable();
        if facts.peek().is_none() {
            return Ok(ty.clone());
        }
        let declared = self.expand_aliases(ty, 0);
        // A name that is no class or alias, as the `T` of `---@generic T` in its function, may hold
        // any value, so the guards leave it as it is, as TypeScript keeps a `T`, and narrow the rest.
        let parts = match &declared {
            Type::Union(parts) => parts.clone(),
            one => vec![one.clone()],
        };
        let (opaque, known): (Vec<Type>, Vec<Type>) = parts.into_iter().partition(|part| self.names_no_type(part));
        if opaque.is_empty() {
            let narrowed = narrowed_by(&declared, facts)?;
            return Ok(if narrowed != declared { narrowed } else { ty.clone() });
        }
        if known.is_empty() {
            return Ok(ty.clone());
        }
        let left = narrowed_by(&Type::union(known), facts).ok();
        Ok(Type::union(left.into_iter().chain(opaque)))
    }

    /// Whether `ty` is a name that names no class or alias the file sees, as a generic does in the
    /// function that declares it.
    fn names_no_type(&self, ty: &Type) -> bool {
        let Type::Named(name, args) = ty else { return false };
        let vector = matches!(name.as_str(), "vector2" | "vector3" | "vector4" | "quat" | "matrix");
        args.is_empty()
            && !vector
            && self.index.class(name, self.side).is_none()
            && self.index.alias(name, self.side).is_none()
    }

    /// `ty`, the shape that the constructor of the table `owner` gives it, with the fields that code
    /// sets through the local inside a function that owns the table, as the indexer records them.
    fn with_owned_fields(&self, ty: Type, owner: &str) -> Type {
        let Type::Shape(shape) = &ty else { return ty };
        let owned = self.members(&Type::GlobalTable(SmolStr::new(owner)));
        let added: Vec<ShapeField> = owned
            .into_iter()
            .filter(|member| !shape.fields.iter().any(|field| field.name == member.name))
            .map(|member| ShapeField { name: member.name, ty: member.ty.widen(), optional: false })
            .collect();
        if added.is_empty() {
            return ty;
        }
        let mut shape = (**shape).clone();
        shape.fields.extend(added);
        Type::Shape(Arc::new(shape))
    }

    /// The names of the fields that the constructor of the table `owner` gives it, as `a` of
    /// `local t = { a = 1 }`, when a local of this file declares the table with a constructor that
    /// sets any entry and no metatable is given to it. `None` for another table, as one that `{}`
    /// builds, which takes any field.
    pub fn constructor_fields(&self, owner: &str) -> Option<Vec<SmolStr>> {
        let decl = self.ctx.local_owner_decl(owner)?;
        let Some(Resolved::Local(id)) = self.ctx.resolution.resolve_at(decl) else { return None };
        let Some(Decl::Local { stmt, index }) = self.ctx.decl(decl) else { return None };
        let StmtKind::Local { exprs, .. } = &stmt.kind else { return None };
        let ExprKind::Table(fields) = &exprs.get(*index)?.kind else { return None };
        if fields.is_empty() || self.ctx.sets_metatable(id) {
            return None;
        }
        let names = fields.iter().filter_map(|field| match field {
            TableField::Named { name, .. } | TableField::SetMember(name) => Some(name.text.clone()),
            TableField::Keyed { key, .. } => key.as_string().cloned(),
            TableField::Positional(_) => None,
        });
        Some(names.collect())
    }

    /// `fact`, or for a comparison with a local, what the type of that local where the comparison
    /// reads it tells, as TypeScript narrows by equality: `cam == activeCam` for an `activeCam` that
    /// is never `nil` or `false` rules those out of `cam`, and one with a local that holds a single
    /// literal tells that `cam` holds it, or with `~=` that it does not.
    fn typed_facts<'f>(&self, fact: &'f Fact) -> [Option<Cow<'f, Fact>>; 2] {
        let (other, at, equal) = match fact {
            Fact::SameAs { other, at } => (*other, *at, true),
            Fact::NotSameAs { other, at } => (*other, *at, false),
            _ => return [Some(Cow::Borrowed(fact)), None],
        };
        let ty = self.guarded(|| self.expand_aliases(&self.local_type_at(other, at), 0));
        let parts = match &ty {
            Type::Union(parts) => &parts[..],
            one => std::slice::from_ref(one),
        };
        let unit = |part: &Type| matches!(part, Type::Nil | Type::BooleanLit(_) | Type::StringLit(_) | Type::IntLit(_));
        if parts.iter().any(Type::is_unknown) || parts.contains(&Type::Any) {
            return [None, None];
        }
        match (parts, equal) {
            ([value], true) if unit(value) => [Some(Cow::Owned(Fact::Is(value.clone()))), None],
            ([value], false) if unit(value) => [Some(Cow::Owned(Fact::IsNot(value.clone()))), None],
            (_, true) => {
                let never_nil = !parts.contains(&Type::Nil);
                let never_false = !parts.iter().any(|part| matches!(part, Type::Boolean | Type::BooleanLit(false)));
                [
                    never_nil.then_some(Cow::Owned(Fact::IsNot(Type::Nil))),
                    never_false.then_some(Cow::Owned(Fact::IsNot(Type::BooleanLit(false)))),
                ]
            }
            (_, false) => [None, None],
        }
    }

    /// `ty`, the type of the field of a local that `expr` reads, as `self.target` or `data.job`,
    /// with what the assignments to it and the guards around the read tell.
    pub fn field_narrowed(&self, expr: &Expr, ty: Type) -> Type {
        let flow = self.ctx.flow();
        let Some(field) = flow.path(self.ctx.resolution, expr) else { return ty };
        self.path_narrowed(field, expr.span.start, ty)
    }

    /// `ty`, the type of the field `key` read from `base`, as `field_narrowed` gives it.
    pub fn member_narrowed(&self, base: &Expr, key: &str, ty: Type) -> Type {
        let flow = self.ctx.flow();
        let Some(field) = flow.member_path(self.ctx.resolution, base, key) else { return ty };
        self.path_narrowed(field, base.span.start, ty)
    }

    /// `ty`, the type of the field tracked as `field`, where it is read at `offset`: for each value
    /// that may reach the read, `ty`, or the part of it that the assignment which gives the value
    /// stores, without what the guards since rule out.
    fn path_narrowed(&self, field: LocalId, offset: u32, ty: Type) -> Type {
        let values = self.ctx.flow().at(field, offset);
        let types = values.iter().map(|value| {
            let base = match value.origin {
                DECLARATION => ty.clone(),
                origin => self.bounded(&ty, &self.stored_value(origin)),
            };
            let facts = value.facts.iter().filter(|(at, _)| *at <= offset).map(|(_, fact)| fact);
            match self.facts_left(&base, facts) {
                Ok(left) | Err(Some(left)) => left,
                Err(None) => base,
            }
        });
        merge_versions(types.collect())
    }

    /// The type of the value that the assignment of `origin` stores in a field, or `unknown` while
    /// it is being found, as in `data.count = data.count + 1` in a loop.
    fn stored_value(&self, origin: u32) -> Type {
        if let Some(ty) = self.stored.borrow().get(&origin) {
            return ty.clone();
        }
        let Origin::Assignment { stmt, index } = self.ctx.flow().origin(origin) else { return Type::Unknown };
        let StmtKind::Assign { exprs, .. } = &stmt.kind else { return Type::Unknown };
        if !self.storing.borrow_mut().insert(origin) {
            return Type::Unknown;
        }
        let (ty, complete) =
            self.complete(|| assigned_value(exprs, index, |expr| self.expr(expr), |expr| self.expr_multi(expr)));
        self.storing.borrow_mut().remove(&origin);
        if complete {
            self.stored.borrow_mut().insert(origin, ty.clone());
        }
        ty
    }

    /// Whether the guards where `id` is read at `offset` rule out `value` for each value of the local
    /// that may be it, `origin` giving the types as for
    /// `narrowed_with`, also where they rule out every value of its type and `narrowed_with` keeps
    /// it whole: `round` is no `nil` in `round and (round == true or i < round)`, whatever else it
    /// holds.
    pub fn rules_out_with(
        &self,
        id: LocalId,
        offset: u32,
        value: &Type,
        origin: &dyn Fn(u32) -> Option<Bases>,
    ) -> bool {
        let holds = |ty: &Type| match self.expand_aliases(ty, 0) {
            Type::Union(parts) => parts.contains(value),
            one => one == *value,
        };
        self.ctx.flow().at(id, offset).iter().all(|version| {
            let Some(bases) = origin(version.origin) else { return true };
            let may_be = |base: &Type| {
                let (ty, left) = self.version_type(id, offset, version, base.clone());
                holds(&ty) || matches!(&left, Ok(left) | Err(Some(left)) if holds(left))
            };
            if !bases.iter().any(may_be) {
                return true;
            }
            let mut facts = version.facts.iter().map(|(_, fact)| fact);
            facts.try_fold(value.clone(), |left, fact| fact.assume(&left)).is_none_or(|left| left != *value)
        })
    }

    /// `ty` as one entry of a `---@cast` line changes it.
    pub fn cast(&self, ty: Type, entry: &CastEntry) -> Type {
        match entry {
            CastEntry::Replace(cast) => cast.clone(),
            // Adding to a type nothing tells leaves a value that may still be anything else, as
            // lua-language-server shows `string|unknown`.
            CastEntry::Add(added) if ty.is_unknown() => added.clone().or_unknown(),
            CastEntry::Add(added) => Type::union([ty, added.clone()]),
            CastEntry::Remove(removed) => {
                let removed = self.expand_aliases(removed, 0);
                let removes = |part: &Type| match &removed {
                    Type::Union(parts) => parts.iter().any(|removed| covers(removed, part)),
                    removed => covers(removed, part),
                };
                match self.expand_aliases(&ty, 0) {
                    Type::Union(parts) if parts.iter().any(removes) => {
                        Type::union(parts.into_iter().filter(|part| !removes(part)))
                    }
                    Type::Union(_) => ty,
                    one if removes(&one) => Type::Unknown,
                    _ => ty,
                }
            }
        }
    }

    /// `ty` with the aliases it names, also in a union, replaced by what they stand for.
    pub fn expand_aliases(&self, ty: &Type, depth: u32) -> Type {
        let resolved = self.resolve_alias(ty);
        match &resolved {
            Type::Union(parts) if depth < 8 => {
                resolved.rebuilt(parts.iter().map(|part| self.expand_aliases(part, depth + 1)))
            }
            _ => resolved,
        }
    }

    /// The type in `code` of a local declared with others by one call, as `err` in
    /// `local ok, err = f()`, when the guards on those locals that hold there rule out some of the
    /// sets of values the call returns: what its position holds in the sets that are left.
    fn linked_type(&self, id: LocalId, code: Span) -> Option<Type> {
        let flow = self.ctx.flow();
        let members = flow.linked(id, DECLARATION)?;
        if !members.iter().any(|(member, origin, _)| flow.facts(*member, *origin, code).next().is_some()) {
            return None;
        }
        let Some(Decl::Local { stmt, .. }) = self.ctx.decl(self.ctx.resolution.local(id).decl.start) else {
            return None;
        };
        // A `---@type` or `---@class` above the statement decides the types instead.
        let doc = self.ctx.doc_at(stmt.span.start);
        if doc.ty.is_some() || !doc.classes.is_empty() {
            return None;
        }
        let sets = self.linked_sets(stmt)?;
        let value = |set: &[Type], position: usize| match set.get(position) {
            Some(ty) => self.expand_aliases(ty, 0),
            None => Type::Nil,
        };
        let is_possible = |set: &&Vec<Type>| {
            members.iter().all(|(member, origin, position)| {
                let value = value(set, *position);
                flow.facts(*member, *origin, code).all(|fact| fact.apply(&value).is_some())
            })
        };
        let possible: Vec<&Vec<Type>> = sets.iter().filter(is_possible).collect();
        if possible.is_empty() || possible.len() == sets.len() {
            return None;
        }
        let position = members.iter().find(|(member, ..)| *member == id)?.2;
        Some(merge_values(possible.iter().map(|set| set.get(position).cloned().unwrap_or(Type::Nil)).collect()))
    }

    /// The sets of values returned by the call that declares the locals of `stmt`, when its function
    /// lists several.
    fn linked_sets(&self, stmt: &Stmt) -> Option<ReturnSets> {
        if let Some(sets) = self.linked_sets.borrow().get(&stmt.span.start) {
            return sets.clone();
        }
        let StmtKind::Local { exprs, .. } = &stmt.kind else { return None };
        let (sets, complete) = self.complete(|| {
            self.guarded(|| match exprs.last().map(|call| &call.kind) {
                Some(ExprKind::Call { callee, args, .. }) => self.call_values(callee, None, args).sets,
                Some(ExprKind::MethodCall { base, method, args, .. }) => {
                    self.call_values(base, Some(method), args).sets
                }
                _ => Vec::new(),
            })
        });
        let sets = (sets.len() > 1).then(|| Rc::new(sets));
        if complete {
            self.linked_sets.borrow_mut().insert(stmt.span.start, sets.clone());
        }
        sets
    }

    pub fn global_type(&self, name: &str) -> Type {
        if name == "exports" {
            return Type::Exports(None);
        }
        let symbols = self.index.globals_named(name, self.ctx.file);
        let known = self.preferred_global(&symbols).map(|(_, s)| &s.ty).filter(|ty| tells_global_value(ty));
        if let Some(ty) = known.filter(|ty| !(matches!(ty, Type::Table) && self.index.has_members(name))) {
            // `local lib = {}` published with `_ENV.lib = lib` and then extended as `function lib.x()`
            // elsewhere keeps its members under two owners.
            let aliased = matches!(ty, Type::GlobalTable(owner) if owner != name) && self.index.has_members(name);
            return match ty {
                _ if aliased => Type::union([ty.clone(), Type::GlobalTable(SmolStr::new(name))]),
                Type::Fun(_) => self.with_fields(ty.clone(), name),
                _ => ty.clone(),
            };
        }
        if self.index.has_members(name) {
            return Type::GlobalTable(SmolStr::new(name));
        }
        match native(name) {
            Some(native) => Type::Fun(Arc::new(native_fun_type(&native.on(self.side)))),
            None => Type::Unknown,
        }
    }

    /// The declaration among `symbols`, those of one global, whose type the global takes: of the
    /// ones that tell its value, one that is or may be a class, as lua-language-server lets a declared
    /// type win, then one of a global table. Then the nearest: in the same file, then in its resource,
    /// then elsewhere, where a class declared with `---@type` comes before a value that may be one, as
    /// `Config = Config or {}` is the `LoafWrapperConfig|table` it declares, and the nearest folder
    /// before the others. Last, the one whose type says most. A value of unknown type that the file
    /// itself assigns tells it too, so that `GetPlayer = ESX.GetPlayerFromId` leaves the calls of its
    /// own file unknown rather than taking another file's `GetPlayer`.
    pub fn preferred_global<'s>(&self, symbols: &'s [(FileId, &'s Symbol)]) -> Option<&'s (FileId, &'s Symbol)> {
        symbols.iter().max_by_key(|(file, symbol)| {
            let own = *file == self.ctx.file && !matches!(symbol.ty, Type::Nil);
            let (place, folders) = self.index.distance(self.ctx.file, *file);
            let declared = matches!(symbol.ty, Type::Named(..));
            let table = table_rank(&symbol.ty);
            let (place, folders) = (std::cmp::Reverse(place), std::cmp::Reverse(folders));
            (own || tells_global_value(&symbol.ty), table, place, declared, folders, symbol.ty.specificity())
        })
    }

    pub fn local_type(&self, id: LocalId) -> Type {
        if let Some(ty) = self.locals.borrow().get(&id) {
            return ty.clone();
        }
        if !self.in_progress.borrow_mut().insert(id) {
            return Type::Unknown;
        }
        // A type that `MAX_DEPTH` cut short may be found whole from a shallower point, as the local
        // at the end of a long chain of locals read from each other is, so it is not cached: which
        // local the cut fell on would depend on which was read first.
        let (ty, complete) = self.complete(|| self.guarded(|| self.compute_local(id)));
        self.in_progress.borrow_mut().remove(&id);
        if complete {
            self.locals.borrow_mut().insert(id, ty.clone());
        }
        ty
    }

    fn compute_local(&self, id: LocalId) -> Type {
        let local = self.ctx.resolution.local(id);
        if local.kind == LocalKind::ImplicitSelf {
            return match self.ctx.decl(local.decl.start) {
                Some(Decl::SelfParam { name }) => self.self_type(name),
                _ => Type::Unknown,
            };
        }
        let Some(decl) = self.ctx.decl(local.decl.start) else { return Type::Unknown };
        match decl {
            Decl::Local { stmt, index } => {
                let reassigned = local.refs.iter().any(|r| r.write);
                self.local_stmt_type(stmt, *index, local.func == 0, reassigned)
            }
            Decl::LocalFunction { stmt, func } => {
                Type::Fun(Arc::new(self.fun_type(func, Some(stmt.span.start), false)))
            }
            Decl::Param { func, index, doc_anchor, expected } => {
                let name = &func.params[*index].text;
                // A function passed to a call takes the `@param` lines above that call's statement, as
                // the handler of `RegisterServerCallback('name', function(source, id) end)` does,
                // also when the statement assigns what the call returns.
                if let Some(anchor) = doc_anchor {
                    let doc = self.ctx.doc_at(*anchor);
                    if let Some(param) = doc.params.iter().find(|p| p.name == *name) {
                        let ty = match param.ty.mentions_self() {
                            true => param.ty.with_self(&self.doc_self_at(*anchor, func)),
                            false => param.ty.clone(),
                        };
                        return if param.optional { ty.optional() } else { ty };
                    }
                }
                expected.as_ref().and_then(|e| self.expected_param(e, *index)).unwrap_or_default()
            }
            Decl::SelfParam { name } => self.self_type(name),
            Decl::NumericFor => Type::Number,
            Decl::GenericFor { stmt, index } => self.for_in_type(stmt, *index),
        }
    }

    /// The type of the value that `origin` gives the local `id`, or `None` while it is being found,
    /// as for an assignment in a loop that reads what the local held before. The values that the
    /// assignments before it which read the local give are found first, and while one is found, those
    /// given after it that are not known yet are left out: the assignments of a loop that each read
    /// what the others give, as `j = j + 3` repeated, would be found again along every order of them
    /// otherwise. Like the types of locals, it is kept once found.
    pub fn origin_type(&self, id: LocalId, origin: u32) -> Option<Type> {
        // A local that is never assigned again holds the value of its declaration, as found once.
        if origin == DECLARATION && !self.ctx.resolution.local(id).refs.iter().any(|r| r.write) {
            return Some(self.local_type(id));
        }
        let key = (id, origin);
        if let Some(ty) = self.origins.borrow().get(&key) {
            return Some(ty.clone());
        }
        // Origins are numbered in the order they are written.
        let later = |(local, other): &(LocalId, u32)| *local == id && *other <= origin;
        if self.origins_in_progress.borrow().iter().any(later) {
            return None;
        }
        self.origins_in_progress.borrow_mut().insert(key);
        for earlier in self.ctx.flow().chained(id).iter().take_while(|earlier| **earlier < origin) {
            self.origin_type(id, *earlier);
        }
        let (ty, complete) = self.complete(|| self.guarded(|| self.compute_origin(id, origin)));
        self.origins_in_progress.borrow_mut().remove(&key);
        if complete {
            self.origins.borrow_mut().insert(key, ty.clone());
        }
        Some(ty)
    }

    fn compute_origin(&self, id: LocalId, origin: u32) -> Type {
        match self.ctx.flow().origin(origin) {
            Origin::Declaration => self.declaration_type(id),
            Origin::Assignment { stmt, index } => {
                let StmtKind::Assign { exprs, .. } = &stmt.kind else { return Type::Unknown };
                let doc = self.ctx.doc_at(stmt.span.start);
                if let Some(ty) = doc.type_at(index).filter(|_| doc.declared_class().is_none()) {
                    return ty.clone();
                }
                // A table that a local declared outside any function with one is given again is that
                // local's own table, which the fields set on it belong to.
                if exprs.get(index).and_then(table_fields).is_some() {
                    let declared = self.declaration_type(id);
                    if self.is_own_table(id, &declared) {
                        return declared;
                    }
                }
                let value = assigned_value(exprs, index, |expr| self.expr(expr), |expr| self.expr_multi(expr));
                match self.annotation(id) {
                    Some(declared) => self.bounded(&declared, &value),
                    // A value of no known type keeps the type the local is declared with, as one of an
                    // annotated local does, unless that only tells it held `nil`.
                    None if value.is_unknown() => match self.local_type(id) {
                        Type::Nil => value,
                        declared => declared,
                    },
                    None => value,
                }
            }
            Origin::Compound { stmt } => match &stmt.kind {
                StmtKind::CompoundAssign { target, op, expr, .. } => self.binary(*op, target, expr),
                _ => Type::Unknown,
            },
            Origin::Function { stmt } => match &stmt.kind {
                StmtKind::Function { func, .. } => {
                    Type::Fun(Arc::new(self.fun_type(func, Some(stmt.span.start), false)))
                }
                _ => Type::Unknown,
            },
            // A `---@cast` line gives the type it names, or adds to the type the local has there or
            // takes from it.
            Origin::Cast(cast) => {
                let Some(cast) = self.ctx.casts().get(cast) else { return Type::Unknown };
                let held = match cast.replaces() {
                    true => Type::Unknown,
                    false => self.local_type_at(id, cast.span.start),
                };
                cast.entries.iter().fold(held, |ty, (entry, _)| self.cast(ty, entry))
            }
            Origin::Others(_) => match self.origin_bases(id, origin) {
                Some(bases) => merge_versions(bases.iter().cloned().collect()),
                None => Type::Unknown,
            },
        }
    }

    /// The types of the values that `origin` gives the local `id`: that of its value, or for a set
    /// of `Origin::Others` those of the values in it, each once. `None` while they are being found.
    pub fn origin_bases(&self, id: LocalId, origin: u32) -> Option<Bases> {
        let flow = self.ctx.flow();
        let Origin::Others(set) = flow.origin(origin) else { return self.origin_type(id, origin).map(Bases::One) };
        if let Some(types) = self.others.borrow().get(&(id, origin)) {
            return Some(Bases::Set(types.clone()));
        }
        let given = flow.others(set).iter().map(|origin| self.other_origin_type(id, *origin));
        let ((types, found), whole) = self.complete(|| distinct_bases(given));
        if found && whole {
            self.others.borrow_mut().insert((id, origin), types.clone());
        }
        (!types.is_empty()).then_some(Bases::Set(types))
    }

    /// The type of the value that `origin` gives the local `id` as code that runs at other times sees
    /// it, as `origin_type` finds it. A table constructor that a `---@type` line gives again to a
    /// local declared outside any function with one is that local's own table there: the line types
    /// the table where it is given, as lua-language-server reads it, while code elsewhere may fill
    /// the local's table with other values.
    fn other_origin_type(&self, id: LocalId, origin: u32) -> Option<Type> {
        if let Origin::Assignment { stmt, index } = self.ctx.flow().origin(origin) {
            let doc = self.ctx.doc_at(stmt.span.start);
            let typed = doc.type_at(index).is_some() && doc.declared_class().is_none();
            if let StmtKind::Assign { exprs, .. } = &stmt.kind {
                if typed && exprs.get(index).and_then(table_fields).is_some() {
                    let declared = self.declaration_type(id);
                    if self.is_own_table(id, &declared) {
                        return Some(declared);
                    }
                }
            }
        }
        self.origin_type(id, origin)
    }

    /// The type that the declaration of the local `id` gives it. One that is assigned again keeps the
    /// literals that a call or another variable gives it, as the values given later have types of
    /// their own, and without a value it holds `nil` until it is given one.
    fn declaration_type(&self, id: LocalId) -> Type {
        let local = self.ctx.resolution.local(id);
        let Some(Decl::Local { stmt, index }) = self.ctx.decl(local.decl.start) else { return self.local_type(id) };
        if !local.refs.iter().any(|r| r.write) {
            return self.local_type(id);
        }
        if self.is_unset(id) {
            return Type::Nil;
        }
        self.local_stmt_type(stmt, *index, local.func == 0, false)
    }

    /// Whether the local `id` is declared without a value or a type, as by `local name`, and assigned
    /// later.
    fn is_unset(&self, id: LocalId) -> bool {
        let local = self.ctx.resolution.local(id);
        let Some(Decl::Local { stmt, index }) = self.ctx.decl(local.decl.start) else { return false };
        let StmtKind::Local { exprs, in_unpack, .. } = &stmt.kind else { return false };
        let valued = *in_unpack || *index < exprs.len() || exprs.last().is_some_and(Expr::is_multi_value);
        let doc = self.ctx.doc_at(stmt.span.start);
        !valued
            && doc.declared_class().is_none()
            && local_annotation(&doc, *index).is_none()
            && local.refs.iter().any(|r| r.write)
    }

    /// The type that the `---@type` above the declaration of the local `id`, or its `@param` line,
    /// gives it, which also bounds the values assigned to it later.
    pub fn annotation(&self, id: LocalId) -> Option<Type> {
        let local = self.ctx.resolution.local(id);
        match self.ctx.decl(local.decl.start)? {
            Decl::Local { stmt, index } => {
                let doc = self.ctx.doc_at(stmt.span.start);
                match doc.declared_class() {
                    Some(class) => Some(own_type(&class.name, &class.generics)),
                    None => local_annotation(&doc, *index),
                }
            }
            Decl::Param { func, index, doc_anchor: Some(anchor), .. } => {
                let doc = self.ctx.doc_at(*anchor);
                let name = &func.params[*index].text;
                doc.params.iter().any(|param| param.name == *name).then(|| self.local_type(id))
            }
            _ => None,
        }
    }

    /// What a local declared as `declared` holds once it is given a value of type `value`: the parts
    /// of `declared` of the kinds of value it may be, as the `string` of a `string?` for `'x'`. A
    /// value of no known kind, or of none of those kinds, which `assign-type-mismatch` reports,
    /// leaves all of it.
    pub fn bounded(&self, declared: &Type, value: &Type) -> Type {
        let fun = FunType::default();
        let Some(kinds) = self.value_kinds(&fun, value, false, 0) else { return declared.clone() };
        let expanded = self.expand_aliases(declared, 0);
        let Type::Union(parts) = &expanded else { return declared.clone() };
        let fits = |part: &&Type| self.value_kinds(&fun, part, false, 0).is_none_or(|part| part & kinds != 0);
        let kept: Vec<Type> = parts.iter().filter(fits).cloned().collect();
        if kept.is_empty() || kept.len() == parts.len() {
            return declared.clone();
        }
        expanded.rebuilt(kept)
    }

    /// The type of the name at `index` of a `local` statement. A name that takes what a call returns,
    /// or the value of another variable or a field, has its type, unless it is `reassigned` after
    /// its declaration: it may then hold other values of that kind, so `"active"|"busy"` becomes
    /// `string`. A literal written out is widened, as `local mode = 'dev'` is a setting to change.
    fn local_stmt_type(&self, stmt: &Stmt, index: usize, top_level: bool, reassigned: bool) -> Type {
        let StmtKind::Local { names, exprs, in_unpack } = &stmt.kind else { return Type::Unknown };
        let doc = self.ctx.doc_at(stmt.span.start);
        if let Some(class) = doc.declared_class() {
            return own_type(&class.name, &class.generics);
        }
        if let Some(ty) = local_annotation(&doc, index) {
            return ty;
        }
        if *in_unpack {
            let base = exprs.first().map(|e| self.expr(e)).unwrap_or_default();
            return self.member(&base, &names[index].name.text).map(|m| m.ty).unwrap_or_default();
        }
        if let Some(expr) = exprs.get(index) {
            if let Some(fields) = table_fields(expr).filter(|_| top_level) {
                let own = Type::GlobalTable(self.ctx.local_owner_key(names[index].name.span.start));
                // `setmetatable({}, Class)` makes an instance of a `---@class`, which is that class,
                // and one built with fields keeps those.
                let metatable = metatable_args(expr).first().map(|mt| self.expr(mt));
                if let Some(class @ Type::Named(..)) = metatable.map(|mt| self.metatable_index(&mt)) {
                    return if fields.is_empty() { class } else { Type::union([own, class]) };
                }
                return own;
            }
            let mut ty = self.first_value(expr);
            // A local inside a function owns the table its constructor builds, as one at the top of
            // the file does, so what code sets through it is a field of the table. With `strict`, as
            // TypeScript reads it, the table has the fields of its constructor alone.
            if !self.strict && matches!(expr.kind, ExprKind::Table(_)) {
                ty = self.with_owned_fields(ty, &self.ctx.local_owner_key(names[index].name.span.start));
            }
            let is_literal = matches!(
                expr.unparen().kind,
                ExprKind::True | ExprKind::False | ExprKind::Number(_) | ExprKind::String(_)
            );
            // The `false|string` of a call keeps its `false`, and `local copy = state` the literals
            // `state` lists, while `local done = false` is a boolean.
            return match (expr.is_call(), reassigned) {
                (true, false) => ty,
                (true, true) => ty.widen_returned(),
                (false, false) if !is_literal => ty,
                (false, _) => ty.widen(),
            };
        }
        match exprs.last() {
            Some(last) if last.is_multi_value() => {
                let ty = value_at(self.expr_multi(last), index + 1 - exprs.len()).unwrap_or_default();
                if reassigned {
                    ty.widen_returned()
                } else {
                    ty
                }
            }
            _ => Type::Unknown,
        }
    }

    /// `lib.onCache('vehicle', function(value, oldValue)`: both parameters are `cache.vehicle`.
    fn on_cache_param(&self, call: &Expr) -> Option<Type> {
        let ExprKind::Call { callee, args, .. } = &call.kind else { return None };
        if callee.dotted_path().as_deref() != Some("lib.onCache") {
            return None;
        }
        let key = args.first()?.as_string()?;
        self.member(&self.global_type("cache"), key).map(|m| m.ty)
    }

    /// The type of parameter `index` of a function literal written where `expected` says.
    fn expected_param(&self, expected: &Expected, index: usize) -> Option<Type> {
        if let Expected::Arg { call, arg_index } = *expected {
            if let Some(ty) = self.on_cache_param(call).filter(|_| index < 2) {
                return Some(ty);
            }
            if let Some(returns) = self.triggered_returns(call, arg_index) {
                return Some(returns.get(index).cloned().unwrap_or(Type::Nil));
            }
        }
        let param = self.expected_fun(expected)?.params.get(index)?.clone();
        Some(if param.optional { param.ty.optional() } else { param.ty })
    }

    /// The type the callee of a call declares for the parameter `id` of a function passed to it, with
    /// the generics of the callee bound from the types `declared` gives its other arguments: `resolve`
    /// of `fun(resolve: fun(value: T))` takes a `boolean` for `promise('boolean', ...)` whose first
    /// parameter is a `` `T` ``. A generic they leave unbound is unknown: the values the call passes
    /// bind it otherwise, which tells nothing declared. So is the parameter when the signatures that
    /// the call fits equally well, such as two `@overload`s for the same action, type it differently.
    /// The `?` of an optional parameter, as the `body?: string` that `PerformHttpRequest` passes,
    /// adds `nil` only with `strict`: lua-language-server gives the parameter the type alone. `None`
    /// when a `@param` line types the parameter.
    pub fn declared_callback_param(&self, id: LocalId, declared: impl Fn(&Expr) -> Type) -> Option<Type> {
        let local = self.ctx.resolution.local(id);
        let Some(Decl::Param { func, index, doc_anchor, expected: Some(Expected::Arg { call, arg_index }) }) =
            self.ctx.decl(local.decl.start)
        else {
            return None;
        };
        let name = &func.params[*index].text;
        if self.ctx.doc_at(doc_anchor.unwrap_or(call.span.start)).params.iter().any(|p| p.name == *name) {
            return None;
        }
        if (*index < 2 && self.on_cache_param(call).is_some()) || self.triggered_returns(call, *arg_index).is_some() {
            return None;
        }
        if let Some(handler) = self.builtin_event_handler(call, *arg_index) {
            let param = handler.params.get(*index)?;
            return Some(if param.optional && self.strict { param.ty.clone().optional() } else { param.ty.clone() });
        }
        let (base, method, exprs) = match &call.kind {
            ExprKind::Call { callee, args, .. } => (callee, None, args),
            ExprKind::MethodCall { base, method, args, .. } => (base, Some(method), args),
            _ => return None,
        };
        let via_method = method.is_some();
        let (fun, _) = self.callee_fun(base, method)?;
        let signatures = self.signatures_at(&fun, call.span.start);
        let inferred = CallArgs::new(exprs);
        let fits: Vec<Fit> = signatures.iter().map(|signature| self.fit(signature, &inferred, via_method)).collect();
        let best = fits.iter().max().copied()?;
        let args = CallArgs::typed(exprs, declared);
        let mut types = signatures.iter().zip(&fits).filter(|(_, fit)| **fit == best).map(|(callee, _)| {
            let (skip_params, skip_args) = callee.call_offsets(via_method);
            let callback = self.fun_of(&callee.params.get((arg_index + skip_params).checked_sub(skip_args)?)?.ty)?;
            let param = callback.params.get(*index)?;
            let ty = substitute(&param.ty, &self.bind_generics(callee, &args, via_method, false));
            Some(if param.optional && self.strict { ty.optional() } else { ty })
        });
        let first = types.next()?;
        if types.all(|ty| ty == first) {
            first
        } else {
            Some(Type::Unknown)
        }
    }

    /// The function type that something declares the function literal `func` has to be: the
    /// parameter of the call it is passed to, the `---@type` of the statement it is the value of,
    /// or the field of a typed table it is written in. A value it returns whose type names a
    /// generic of the callee or of the type itself is `any`, as only the arguments of a call bind
    /// those, which declares nothing. `None` when nothing declares one, or when the call fits
    /// several signatures, which may take different functions.
    pub fn declared_fun_type(&self, func: &FuncBody) -> Option<Arc<FunType>> {
        let expected = self.ctx.functions.get(&func.params_span.start)?;
        let (ty, callee_generics) = match *expected {
            Expected::Arg { call, arg_index } if self.builtin_event_handler(call, arg_index).is_some() => {
                (self.expected_type(expected)?, Vec::new())
            }
            Expected::Arg { call, arg_index } => {
                let (base, method, args) = match &call.kind {
                    ExprKind::Call { callee, args, .. } => (callee, None, args),
                    ExprKind::MethodCall { base, method, args, .. } => (base, Some(method), args),
                    _ => return None,
                };
                let via_method = method.is_some();
                let (callee, _) = self.callee_fun(base, method)?;
                let args = CallArgs::new(args);
                let signatures = self.signatures_at(&callee, call.span.start);
                let mut fitting =
                    signatures.iter().filter(|signature| self.fit(signature, &args, via_method) != Fit::No);
                let (Some(signature), None) = (fitting.next(), fitting.next()) else { return None };
                let (skip_params, skip_args) = signature.call_offsets(via_method);
                let param = signature.params.get((arg_index + skip_params).checked_sub(skip_args)?)?;
                (param.ty.clone(), signature.generics.clone())
            }
            _ => (self.expected_type(expected)?, Vec::new()),
        };
        let fun = self.sole_fun(&ty)?;
        let generics = FunType {
            generics: callee_generics.into_iter().chain(fun.generics.iter().cloned()).collect(),
            ..FunType::default()
        };
        let declared = |types: &[Type]| -> Vec<Type> {
            types.iter().map(|ty| if self.names_generic(&generics, ty, 0) { Type::Any } else { ty.clone() }).collect()
        };
        Some(Arc::new(FunType {
            returns: declared(&fun.returns),
            return_sets: fun.return_sets.iter().map(|set| declared(set)).collect(),
            overloads: Vec::new(),
            ..(*fun).clone()
        }))
    }

    /// The one function type that `ty` describes, through aliases and beside other kinds of value,
    /// as in `fun()?` or `table|fun()`. `None` when it lists several, like the two signatures of a
    /// searcher that returns `function` or `nil, string`, which a function may be either of.
    fn sole_fun(&self, ty: &Type) -> Option<Arc<FunType>> {
        match self.expand_aliases(ty, 0) {
            Type::Fun(fun) => Some(fun),
            Type::Union(parts) => {
                let mut funs = parts.into_iter().filter_map(|part| match part {
                    Type::Fun(fun) => Some(fun),
                    _ => None,
                });
                match (funs.next(), funs.next()) {
                    (Some(fun), None) => Some(fun),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Whether `ty` names one of the generics of `fun`.
    fn names_generic(&self, fun: &FunType, ty: &Type, depth: u32) -> bool {
        if depth > 8 {
            return false;
        }
        let names = |ty: &Type| self.names_generic(fun, ty, depth + 1);
        match ty {
            Type::Named(name, args) => (args.is_empty() && self.is_generic(fun, name)) || args.iter().any(names),
            Type::NameOf(_) => true,
            Type::Array(inner) | Type::Variadic(inner) => names(inner),
            Type::Map(key, value) => names(key) || names(value),
            Type::Tuple(types) | Type::Union(types) => types.iter().any(names),
            Type::Fun(inner) => inner.returns.iter().any(names) || inner.params.iter().any(|param| names(&param.ty)),
            Type::Shape(shape) => {
                shape.fields.iter().any(|field| names(&field.ty))
                    || shape.array.as_ref().is_some_and(names)
                    || shape.indices.iter().any(|(key, value)| names(key) || names(value))
            }
            _ => false,
        }
    }

    /// `TriggerCallback('name', function(response) end)` receives what the handler of `name`
    /// returns, when argument `arg_index` of `call` is that function.
    fn triggered_returns(&self, call: &Expr, arg_index: usize) -> Option<Vec<Type>> {
        let (fun, args, via_method) = self.call_parts(call)?;
        let wrapper = Wrapper::of(&fun, via_method).filter(|w| w.tag.role == CallbackRole::Trigger)?;
        if wrapper.function.map(|param| wrapper.arg(param)) != Some(arg_index) {
            return None;
        }
        let handler = self.wrapper_handler(&wrapper, args.exprs, call.span.start)?;
        (!handler.returns.is_empty()).then(|| handler.returns.clone())
    }

    /// The handler that `AddEventHandler('onResourceStop', function(resourceName) end)` registers
    /// for an event that FiveM itself triggers on the side of the call, when argument `arg_index` of
    /// `call` is that function.
    fn builtin_event_handler(&self, call: &Expr, arg_index: usize) -> Option<Arc<FunType>> {
        let ExprKind::Call { callee, args, .. } = &call.kind else { return None };
        let registers = matches!(
            callee.dotted_path().as_deref(),
            Some("AddEventHandler" | "RegisterNetEvent" | "RegisterServerEvent")
        );
        if arg_index != 1 || !registers {
            return None;
        }
        let name = args.first()?.as_string()?;
        let side = self.side_at(call.span.start);
        let mut events = self.index.builtin_events();
        let (field, _) = events.find(|(field, field_side)| field.name == *name && applies_on(*field_side, side))?;
        self.fun_of(&field.ty)
    }

    /// The function type that a function literal written where `expected` says has to be.
    pub fn expected_fun(&self, expected: &Expected) -> Option<Arc<FunType>> {
        self.fun_of(&self.expected_type(expected)?)
    }

    /// The type that a value written where `expected` says has to be, when something declares it.
    fn expected_type(&self, expected: &Expected) -> Option<Type> {
        match *expected {
            Expected::Arg { call, arg_index } => {
                if let Some(handler) = self.builtin_event_handler(call, arg_index) {
                    return Some(Type::Fun(handler));
                }
                let (fun, args, via_method) = self.call_parts(call)?;
                let (skip_params, skip_args) = fun.call_offsets(via_method);
                let param = &fun.params.get((arg_index + skip_params).checked_sub(skip_args)?)?.ty;
                // A table passed for a parameter like the `T` of `setmetatable(t: T)` decides that
                // generic itself. A function literal never does: `bind_generics` leaves those out.
                let is_function = matches!(args.exprs.get(arg_index)?.kind, ExprKind::Function(_));
                if !is_function && self.can_bind(&fun, param) {
                    return None;
                }
                Some(substitute(param, &self.bind_generics(&fun, &args, via_method, false)))
            }
            Expected::Value { stmt, index } => {
                // `---@class Name` above a table declares the class rather than a value of it,
                // unless a `---@type` follows it.
                let doc = self.ctx.doc_at(stmt.span.start);
                if doc.declared_class().is_some() {
                    return None;
                }
                match &stmt.kind {
                    StmtKind::Local { .. } => local_annotation(&doc, index),
                    StmtKind::Assign { exprs, .. } => doc.type_at(index).cloned().or_else(|| {
                        let func = alternative_function(exprs.get(index)?)?;
                        self.field_signature(stmt, func)
                    }),
                    _ => doc.type_at(index).cloned(),
                }
            }
            Expected::Member { stmt } => match &stmt.kind {
                StmtKind::Function { func, .. } => self.field_signature(stmt, func),
                _ => None,
            },
            Expected::Field { table, key } => {
                let at = self.ctx.tables.get(&table.span.start)?;
                let table = self.guarded(|| self.expected_type(at))?;
                self.field_type(&table, key)
            }
            Expected::Return { func, doc_anchor, index } => {
                // Only what is declared counts: the returns inferred from the body would read the
                // function literal that is being typed.
                let documented = doc_anchor.map(|anchor| (anchor, self.ctx.doc_at(anchor)));
                match documented.filter(|(_, doc)| !doc.returns.is_empty()) {
                    Some((anchor, doc)) => {
                        let ty = &doc.returns.get(index)?.ty;
                        Some(match ty.mentions_self() {
                            true => ty.with_self(&self.doc_self_at(anchor, func)),
                            false => ty.clone(),
                        })
                    }
                    None => self.guarded(|| self.declared_fun_type(func))?.returns.get(index).cloned(),
                }
            }
        }
    }

    /// The `fun(...)` that `func`, which `stmt` puts in a field of a table, implements: the type the
    /// class of the table declares for the field with `@field`. It counts when `func` takes as many
    /// parameters as it lists, besides the `self` that a method declared with `:` takes itself.
    fn field_signature(&self, stmt: &Stmt, func: &FuncBody) -> Option<Type> {
        let (name, is_method) = match &stmt.kind {
            StmtKind::Function { name, .. } => {
                (&name.method.as_ref().or(name.path.last())?.text, name.method.is_some())
            }
            StmtKind::Assign { targets, exprs } => {
                let defines = |expr: &Expr| alternative_function(expr).is_some_and(|f| std::ptr::eq(f, func));
                match &targets.get(exprs.iter().position(defines)?)?.kind {
                    ExprKind::Field { name, .. } => (&name.text, false),
                    ExprKind::Index { index, .. } => (index.as_string()?, false),
                    _ => return None,
                }
            }
            _ => return None,
        };
        let fun = self.fun_of(&self.declared_field(&self.doc_self(stmt, func), name)?)?;
        let skip = usize::from(is_method && fun.params.first().is_some_and(|param| param.name == "self"));
        let takes = func.params.len() + usize::from(func.vararg.is_some());
        (fun.params.len() == skip + takes)
            .then(|| Type::Fun(Arc::new(FunType { params: fun.params[skip..].to_vec(), ..(*fun).clone() })))
    }

    /// The type of the `@field` called `name` that the class `owner` is, or a parent, declares,
    /// leaving out what code sets on the tables of the class.
    fn declared_field(&self, owner: &Type, name: &str) -> Option<Type> {
        let declared = |member: &MemberInfo| {
            member.location.is_some_and(|(file, range)| {
                let classes = self.index.file(file).map(|entry| entry.index.classes.as_slice()).unwrap_or_default();
                classes.iter().any(|class| class.fields.iter().any(|field| field.name == name && field.range == range))
            })
        };
        self.members_named(owner, name).into_iter().find(declared).map(|member| member.ty)
    }

    /// The signature that `call` uses, with its arguments and whether it calls a method.
    fn call_parts<'e>(&self, call: &'e Expr) -> Option<(Arc<FunType>, CallArgs<'e>, bool)> {
        let (base, method, args) = match &call.kind {
            ExprKind::Call { callee, args, .. } => (callee, None, args),
            ExprKind::MethodCall { base, method, args, .. } => (base, Some(method), args),
            _ => return None,
        };
        let via_method = method.is_some();
        let (fun, _) = self.callee_fun(base, method)?;
        let args = CallArgs::new(args);
        let fun = self.signature_for(&fun, &args, via_method, call.span.start);
        Some((fun, args, via_method))
    }

    /// The type of the field `key` in a table of type `table`.
    fn field_type(&self, table: &Type, key: FieldKey) -> Option<Type> {
        let key = match key {
            FieldKey::Name(name) => {
                if let Some(member) = self.member(table, name) {
                    return Some(member.ty);
                }
                Type::StringLit(SmolStr::new(name))
            }
            FieldKey::Position(position) => Type::IntLit(position),
            FieldKey::Expr(key) => self.expr(key),
        };
        Some(self.element_type(&table.without_nil(), &key, 0)).filter(|ty| !ty.is_unknown())
    }

    /// The function type `ty` describes, through aliases and the `nil` of `fun()?`.
    pub fn fun_of(&self, ty: &Type) -> Option<Arc<FunType>> {
        match self.resolve_alias(ty) {
            Type::Fun(fun) => Some(fun),
            Type::Union(types) => types.iter().find_map(|part| self.guarded(|| self.fun_of(part))),
            _ => None,
        }
    }

    /// Whether `name` is the table a `---@class` annotation declares, like `Test` in `---@class Test`
    /// `local Test = {}`, rather than a value typed as that class.
    pub fn is_class_table(&self, name: &Name) -> bool {
        match self.ctx.resolution.resolve_at(name.span.start) {
            Some(Resolved::Local(id)) => match self.ctx.decl(self.ctx.resolution.local(id).decl.start) {
                Some(Decl::Local { stmt, .. }) => self.ctx.doc_at(stmt.span.start).declared_class().is_some(),
                _ => false,
            },
            _ => self.index.globals_named(&name.text, self.ctx.file).iter().any(|(_, symbol)| symbol.is_class_table()),
        }
    }

    /// The type `self` has inside `function a.b:c()`, which is also the owner of `c`.
    pub fn func_name_owner_type(&self, name: &FuncName) -> Type {
        self.path_type(&name.base, &name.path)
    }

    /// The type of `base.path`, as of `a.b` for `base` `a` and `path` `b`.
    fn path_type(&self, base: &Name, path: &[Name]) -> Type {
        let mut ty = self.name(base);
        for segment in path {
            ty = self.member(&ty, &segment.text).map(|m| m.ty).unwrap_or_default();
        }
        ty
    }

    /// The class that `self` stands for in the doc comment above `stmt`, for `func`, one of the
    /// functions it defines: the type of the table that `function a.b:c()`, `function a.b.c()` or
    /// `a.b.c = function()` puts the function in, `a.b`. Unknown for other functions, such as a
    /// `local function`.
    pub fn doc_self(&self, stmt: &Stmt, func: &FuncBody) -> Type {
        match &stmt.kind {
            StmtKind::Function { name, func: defined } if std::ptr::eq(&**defined, func) => match name.method {
                Some(_) => self.func_name_owner_type(name),
                None => match name.path.split_last() {
                    Some((_, owner)) => self.path_type(&name.base, owner),
                    None => Type::Unknown,
                },
            },
            StmtKind::Assign { targets, exprs } => {
                let defines = |expr: &Expr| alternative_function(expr).is_some_and(|f| std::ptr::eq(f, func));
                match exprs.iter().position(defines).and_then(|index| targets.get(index)).map(|target| &target.kind) {
                    Some(ExprKind::Field { base, .. } | ExprKind::Index { base, .. }) => self.expr(base),
                    _ => Type::Unknown,
                }
            }
            _ => Type::Unknown,
        }
    }

    /// `ty`, read from the doc comment above `stmt` for `func`, one of the functions it defines,
    /// with `self` standing for the class `doc_self` gives.
    pub fn doc_type_for(&self, stmt: &Stmt, func: &FuncBody, ty: &Type) -> Type {
        match ty.mentions_self() {
            true => ty.with_self(&self.doc_self(stmt, func)),
            false => ty.clone(),
        }
    }

    /// The class that `self` stands for in the doc comment of the statement at `anchor`, for `func`.
    fn doc_self_at(&self, anchor: u32, func: &FuncBody) -> Type {
        statement_at(self.ctx.chunk, anchor).map_or(Type::Unknown, |stmt| self.doc_self(stmt, func))
    }

    /// The type of the `self` of a function defined with `:`: a value of the table it is defined on,
    /// or a string for a method added to the `string` library, which strings call.
    fn self_type(&self, name: &FuncName) -> Type {
        let on_strings = name.base.text == "string"
            && name.path.is_empty()
            && matches!(self.ctx.resolution.resolve_at(name.base.span.start), Some(Resolved::Global(_)));
        if on_strings {
            return Type::String;
        }
        self.func_name_owner_type(name)
    }

    fn for_in_type(&self, stmt: &Stmt, index: usize) -> Type {
        let StmtKind::GenericFor { exprs, .. } = &stmt.kind else { return Type::Unknown };
        let Some(first) = exprs.first() else { return Type::Unknown };
        if let Some((arg, ipairs)) = iterated_table(stmt) {
            let (key, value) = match &arg.unparen().kind {
                ExprKind::Table(fields) => self.inline_table_key_values(fields, ipairs),
                _ => self.key_value_types(&self.expr(arg), ipairs),
            };
            let key = if ipairs { Type::Integer } else { key };
            return if index == 0 {
                key
            } else if index == 1 {
                value
            } else {
                Type::Unknown
            };
        }
        self.expr(first).as_fun().and_then(|f| f.returns.get(index).cloned()).unwrap_or_default()
    }

    /// `pairs({ 'male', 'female' })`: nothing else can reach a table built in the call, so its keys
    /// and values keep their literal types instead of widening to `string`.
    fn inline_table_key_values(&self, fields: &[TableField], array_only: bool) -> (Type, Type) {
        let fields = &fields[..fields.len().min(MAX_SHAPE_FIELDS)];
        let elements = table_elements(fields);
        let mut keys = Vec::new();
        let mut values = Vec::new();
        for field in fields.iter().filter(|_| !array_only) {
            let (key, value) = match field {
                TableField::Named { name, value } => (Type::StringLit(name.text.clone()), self.expr(value)),
                TableField::Keyed { key, value } if matches!(key.kind, ExprKind::String(_)) => {
                    (self.expr(key), self.expr(value))
                }
                TableField::SetMember(name) => (Type::StringLit(name.text.clone()), Type::BooleanLit(true)),
                TableField::Keyed { .. } | TableField::Positional(_) => continue,
            };
            keys.push(key);
            values.push(value);
        }
        for value in elements.array {
            keys.push(Type::Integer);
            values.push(self.expr(value));
        }
        for (key, value) in elements.keyed.into_iter().filter(|_| !array_only) {
            keys.push(self.expr(key));
            values.push(self.expr(value));
        }
        (Type::union(keys), Type::union(values))
    }

    /// What `pairs` yields for a value of type `ty`, or with `array_only` what `ipairs` yields.
    pub fn key_value_types(&self, ty: &Type, array_only: bool) -> (Type, Type) {
        let resolved = self.resolve_alias(ty);
        let (keys, values): (Vec<Type>, Vec<Type>) = match resolved {
            Type::Array(inner) => return (Type::Integer, *inner),
            Type::Tuple(items) => return (Type::Integer, Type::union(items)),
            Type::Map(k, v) => return (*k, *v),
            Type::Shape(shape) => {
                let fields = shape.fields.iter().filter(|_| !array_only).map(|f| (Type::String, f.ty.clone()));
                let array = shape.array.iter().map(|value| (Type::Integer, value.clone()));
                let index = shape.indices.iter().filter(|(key, _)| !array_only || is_integer_key(key)).cloned();
                fields.chain(array).chain(index).unzip()
            }
            Type::Named(ref name, ref args) => {
                let class = self.index.class(name, self.side).map(|(_, c)| c);
                let mut indices = Vec::new();
                self.class_indices(name, args, &mut indices, &mut FxHashSet::default(), 0);
                let index = indices.into_iter().filter(|(key, _)| !array_only || is_integer_key(key));
                // `---@field [1] number` fields, with their keys shown as `integer` rather than `1|2|3`.
                let bindings = self.class_bindings_of(name, args);
                let literal_fields = class
                    .into_iter()
                    .flat_map(|c| c.literal_fields(self.side))
                    .filter(|(key, _)| !array_only || is_integer_key(key))
                    .map(|(key, value)| (key.widen(), substitute(value, &bindings)));
                // An instance holds its data; the methods its class provides are not visited.
                let fields = self
                    .members(&resolved)
                    .into_iter()
                    .filter(|m| !array_only && !matches!(m.kind, SymbolKind::Method | SymbolKind::Function))
                    .map(|m| (Type::String, m.ty));
                let (keys, values): (Vec<Type>, Vec<Type>) = fields.chain(literal_fields).chain(index).unzip();
                // A class without fields still names them with strings, while a name that is no
                // class, as the `T` of `---@generic T: table` in its function, tells nothing about
                // either, as a plain `table` does not.
                match (keys.is_empty(), class) {
                    (true, Some(_)) => return (Type::String, Type::Unknown),
                    (true, None) => return (Type::Unknown, Type::Unknown),
                    (false, _) => {}
                }
                (keys, values)
            }
            Type::GlobalTable(owner) => return self.global_table_key_values(&owner, array_only),
            Type::Union(types) => types
                .iter()
                .filter(|t| !matches!(t, Type::Nil))
                .map(|t| self.guarded(|| self.key_value_types(t, array_only)))
                .unzip(),
            _ => return (Type::Unknown, Type::Unknown),
        };
        (Type::union(keys), Type::union(values))
    }

    /// The `[key]` entries of the class `name`, given the type arguments `args`, and of its parents,
    /// with those of a table type it names as a parent: the `[number]: T` of `{ [number]: T }`, or
    /// the keys and values of `table<string, integer>`. A class that several parents share is
    /// read once.
    fn class_indices(
        &self,
        name: &str,
        args: &[Type],
        out: &mut Vec<(Type, Type)>,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) {
        if depth > 8 || !visited.insert(SmolStr::new(name)) {
            return;
        }
        let mut defs = self.index.class_defs(name);
        defs.retain(|(_, def)| applies_on(def.side, self.side));
        let bindings = class_bindings(&defs, args);
        for (key, value) in defs.iter().flat_map(|(_, def)| def.indices(self.side)) {
            out.push((substitute(key, &bindings), substitute(value, &bindings)));
        }
        for (_, parent) in bound_parents(&defs, args) {
            match parent {
                Type::Named(parent, args) => self.class_indices(&parent, &args, out, visited, depth + 1),
                Type::Shape(shape) => {
                    out.extend(shape.array.iter().map(|value| (Type::Integer, value.clone())));
                    out.extend(shape.indices.iter().cloned());
                }
                Type::Map(key, value) => out.push((*key, *value)),
                Type::Array(value) => out.push((Type::Integer, *value)),
                _ => {}
            }
        }
    }

    /// Keys and values of a table the index holds: its named fields are members, its array part and
    /// other keys are elements. A top-level local of this file reads those from its constructor
    /// instead, which is current even while the index still has the file's previous text.
    fn global_table_key_values(&self, owner: &SmolStr, array_only: bool) -> (Type, Type) {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        if !array_only {
            // `pairs` visits the table's own fields, not those its metatable's `__index` gives.
            let mut members = Vec::new();
            self.guarded(|| self.own_members(owner, None, &mut members));
            let mut seen = FxHashSet::default();
            for member in members.into_iter().filter(|m| seen.insert(m.name.clone())) {
                keys.push(Type::String);
                values.push(member.ty);
            }
        }
        match self.local_table_fields(owner) {
            Some(fields) => {
                let elements = table_elements(&fields[..fields.len().min(MAX_SHAPE_FIELDS)]);
                for value in elements.array {
                    keys.push(Type::Integer);
                    values.push(self.expr(value).widen());
                }
                for (key, value) in elements.keyed.into_iter().filter(|_| !array_only) {
                    keys.push(self.expr(key).widen());
                    values.push(self.expr(value).widen());
                }
            }
            None => {
                for element in self.index.elements_of(owner, self.ctx.file) {
                    let key = match &element.key {
                        None => Type::Integer,
                        Some(_) if array_only => continue,
                        Some(key) => key.clone(),
                    };
                    keys.push(key);
                    values.push(element.value.clone());
                }
            }
        }
        if keys.is_empty() {
            return (Type::String, Type::Unknown);
        }
        (Type::union(keys), Type::union(values))
    }

    fn local_table_fields(&self, owner: &str) -> Option<&'a [TableField]> {
        let Decl::Local { stmt, index } = self.ctx.decl(self.ctx.local_owner_decl(owner)?)? else { return None };
        let StmtKind::Local { exprs, .. } = &stmt.kind else { return None };
        exprs.get(*index).and_then(table_fields)
    }

    pub fn resolve_alias(&self, ty: &Type) -> Type {
        let mut current = ty.clone();
        for _ in 0..8 {
            match &current {
                // `Box<integer>` is the type of `---@alias Box<T> { value: T }` with `integer` for `T`.
                Type::Named(name, args) if self.index.class(name, self.side).is_none() => {
                    match self.index.alias(name, self.side) {
                        Some((_, alias)) => current = substitute(&alias.ty, &expanded_bindings(&alias.generics, args)),
                        None => break,
                    }
                }
                Type::Require(path) => match self.module_type(path) {
                    Some(ty) => current = ty,
                    None => break,
                },
                _ => break,
            }
        }
        current
    }

    /// The type parameters of the class `name` bound to the type arguments `args`, as a declaration
    /// of it for this side lists them: `---@class (client) Box<U>` binds `U`.
    fn class_bindings_of(&self, name: &str, args: &[Type]) -> Vec<(SmolStr, Type)> {
        let mut defs = self.index.class_defs(name);
        defs.retain(|(_, def)| applies_on(def.side, self.side));
        class_bindings(&defs, args)
    }

    fn module_type(&self, path: &str) -> Option<Type> {
        let file = self.index.resolve_require(path, self.ctx.file)?;
        self.index.file(file)?.index.module_return.clone()
    }

    fn index_expr(&self, base: &Expr, index: &Expr) -> Type {
        let base_ty = self.expr(base);
        // `byGender[gender]` reads the fields that the literals `gender` may hold name, although
        // the local is a `string`, as lua-language-server reads it. A `nil` key reads no value of
        // the table.
        let key_ty = match self.literal_values(index, 0) {
            Some(values) => Type::union(values),
            None => self.expr(index).without_nil(),
        };
        let mut keys = Vec::new();
        if self.literal_keys(&key_ty, 0, &mut keys) && !keys.is_empty() {
            let fields: Option<Vec<Type>> = keys.iter().map(|key| self.member(&base_ty, key).map(|m| m.ty)).collect();
            if let Some(fields) = fields {
                return Type::union(fields);
            }
        }
        self.element_type(&base_ty.without_nil(), &key_ty, 0)
    }

    /// The literals that `expr` may give: `"male"` and `"female"` for `isMale and "male" or
    /// "female"`, also through a local without `---@type` that is never assigned again and holds
    /// one, as `local gender = isMale and "male" or "female"` does. `None` when it may give
    /// another value.
    fn literal_values(&self, expr: &Expr, depth: u32) -> Option<Vec<Type>> {
        if depth > MAX_DEPTH {
            return None;
        }
        match &expr.unparen().kind {
            ExprKind::String(_) | ExprKind::Number(_) | ExprKind::True => {
                let ty = self.expr(expr);
                ty.is_literal().then(|| vec![ty])
            }
            // `a and "x" or "y"` gives `"x"` when `a` holds, and `"y"` otherwise.
            ExprKind::Binary { op: BinOp::Or, lhs, rhs, .. } => {
                let mut values = match &lhs.unparen().kind {
                    ExprKind::Binary { op: BinOp::And, rhs: value, .. } => self.literal_values(value, depth + 1)?,
                    _ => self.literal_values(lhs, depth + 1)?,
                };
                values.extend(self.literal_values(rhs, depth + 1)?);
                Some(values)
            }
            ExprKind::Name(name) => {
                let Some(Resolved::Local(id)) = self.ctx.resolution.resolve_at(name.span.start) else { return None };
                let local = self.ctx.resolution.local(id);
                if local.refs.iter().any(|r| r.write) {
                    return None;
                }
                let Some(Decl::Local { stmt, index }) = self.ctx.decl(local.decl.start) else { return None };
                let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return None };
                if local_annotation(&self.ctx.doc_at(stmt.span.start), *index).is_some() {
                    return None;
                }
                self.literal_values(exprs.get(*index)?, depth + 1)
            }
            _ => None,
        }
    }

    /// What `base[key]` holds for a key of type `key_ty` that is not a known field name.
    fn element_type(&self, base: &Type, key_ty: &Type, depth: u32) -> Type {
        match self.resolve_alias(base) {
            Type::Union(types) if depth < 8 => Type::union(
                types.iter().filter(|t| !matches!(t, Type::Nil)).map(|t| self.element_type(t, key_ty, depth + 1)),
            ),
            Type::Array(inner) => *inner,
            Type::Map(_, value) => *value,
            Type::Tuple(items) => match key_ty {
                Type::IntLit(i) => items.get((*i as usize).wrapping_sub(1)).cloned().unwrap_or_default(),
                _ => Type::union(items),
            },
            // The array part and the `[key]` entries that take a key of its type.
            shape @ Type::Shape(_) => self.table_index_value(&shape, key_ty),
            Type::Named(name, args) => {
                let Some((_, class)) = self.index.class(&name, self.side) else { return Type::Unknown };
                let key_ty = self.resolve_alias(key_ty);
                let index = self.class_index_value(&name, &args, &key_ty, &mut FxHashSet::default(), 0);
                let bindings = self.class_bindings_of(&name, &args);
                // `employee[1]` reads the `---@field [1] number` of its key, and `employee[i]` any
                // literal-keyed field that `i` may name.
                if key_ty.is_literal() {
                    let field = class.literal_fields(self.side).find(|(key, _)| **key == key_ty);
                    return field.map_or(index, |(_, value)| substitute(value, &bindings));
                }
                let fields = class.literal_fields(self.side).filter(|(key, _)| may_be_literal_key(&key_ty, key));
                Type::union(fields.map(|(_, value)| substitute(value, &bindings)).chain([index]))
            }
            // `list[i]` on a table whose array part the index or a top-level local's constructor holds.
            Type::GlobalTable(owner) if is_number_key(&key_ty.without_nil().widen()) => {
                self.global_table_key_values(&owner, true).1
            }
            Type::String | Type::StringLit(_) => Type::Unknown,
            _ => Type::Unknown,
        }
    }

    /// Whether an index declared for keys of type `declared`, such as the `[string]` of
    /// `{ [string]: integer }`, takes a key of type `key`: one of a kind it takes, and when both are
    /// literals, the same. An index of `'a'|'b'`, or of an alias of them, takes each of the two.
    fn index_takes(&self, declared: &Type, key: &Type, depth: u32) -> bool {
        if depth > 8 {
            return true;
        }
        let (declared, key) = (self.resolve_alias(declared), self.resolve_alias(key));
        if let Type::Union(parts) = &declared {
            return parts.iter().any(|part| self.index_takes(part, &key, depth + 1));
        }
        if declared.is_literal() && key.is_literal() {
            return declared == key;
        }
        let any = FunType::default();
        match (self.value_kinds(&any, &declared, false, 0), self.value_kinds(&any, &key, false, 0)) {
            (Some(taken), Some(given)) => taken & given != 0,
            _ => true,
        }
    }

    /// What a key of type `key` reads from an instance of `class`, given the type arguments `args`,
    /// through the nearest indices of the class or its parents that take it, including those of a
    /// table type it names as a parent, like `{ [number]: T }`. A class that several parents share
    /// is read once.
    fn class_index_value(
        &self,
        class: &str,
        args: &[Type],
        key: &Type,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) -> Type {
        if depth > 8 || !visited.insert(SmolStr::new(class)) {
            return Type::Unknown;
        }
        let mut defs = self.index.class_defs(class);
        defs.retain(|(_, def)| applies_on(def.side, self.side));
        let bindings = class_bindings(&defs, args);
        let indices = defs.iter().flat_map(|(_, def)| def.indices(self.side));
        let taking = indices.filter(|(index, _)| self.index_takes(&substitute(index, &bindings), key, 0));
        let own = Type::union(taking.map(|(_, value)| substitute(value, &bindings)));
        if !own.is_unknown() {
            return own;
        }
        let parents = bound_parents(&defs, args).into_iter().map(|(_, parent)| parent);
        Type::union(parents.map(|parent| match parent {
            Type::Named(parent, args) => self.class_index_value(&parent, &args, key, visited, depth + 1),
            table => self.table_index_value(&table, key),
        }))
    }

    /// What a key of type `key` reads from a value of the table type `ty`: the array part and the
    /// `[key]` entries of `{ [string]: integer, 'a' }` that take it, or the values of
    /// `table<string, integer>` or `integer[]`.
    fn table_index_value(&self, ty: &Type, key: &Type) -> Type {
        let takes = |index: &Type| self.index_takes(index, key, 0);
        match ty {
            Type::Shape(shape) => {
                let array = shape.array.iter().filter(|_| takes(&Type::Integer));
                let indices = shape.indices.iter().filter(|(index, _)| takes(index));
                Type::union(array.chain(indices.map(|(_, value)| value)).cloned())
            }
            Type::Map(index, value) if takes(index) => (**value).clone(),
            Type::Array(value) if takes(&Type::Integer) => (**value).clone(),
            _ => Type::Unknown,
        }
    }

    /// The field names a key can be: `'male'`, or every name of a `"male"|"female"` loop variable or
    /// alias. False when any part of the key is not a string literal.
    fn literal_keys(&self, key: &Type, depth: u32, out: &mut Vec<SmolStr>) -> bool {
        if depth > 8 {
            return false;
        }
        match self.resolve_alias(key) {
            Type::StringLit(name) => {
                out.push(name);
                true
            }
            Type::Union(types) => {
                types.iter().filter(|t| !matches!(t, Type::Nil)).all(|t| self.literal_keys(t, depth + 1, out))
            }
            _ => false,
        }
    }

    fn table(&self, fields: &[TableField]) -> Type {
        if fields.is_empty() {
            return Type::Table;
        }
        let fields = &fields[..fields.len().min(MAX_SHAPE_FIELDS)];
        let mut shape = Shape::default();
        for field in fields {
            let (name, ty) = match field {
                TableField::Named { name, value } => (name.text.clone(), self.first_value(value).widen()),
                TableField::Keyed { key: Expr { kind: ExprKind::String(name), .. }, value } => {
                    (name.clone(), self.first_value(value).widen())
                }
                TableField::SetMember(name) => (name.text.clone(), Type::Boolean),
                TableField::Keyed { .. } | TableField::Positional(_) => continue,
            };
            shape.fields.push(ShapeField { name, ty, optional: false });
        }
        let elements = table_elements(fields);
        let array = Type::union(elements.array.iter().map(|value| self.expr(value).widen()));
        if shape.fields.is_empty() && elements.keyed.is_empty() {
            return Type::Array(Box::new(array));
        }
        if !elements.array.is_empty() {
            shape.array = Some(array);
        }
        if !elements.keyed.is_empty() {
            let (keys, values): (Vec<Type>, Vec<Type>) =
                elements.keyed.iter().map(|(key, value)| (self.expr(key).widen(), self.expr(value).widen())).unzip();
            shape.indices.push((Type::union(keys), Type::union(values)));
        }
        Type::Shape(Arc::new(shape))
    }

    /// What `setmetatable(t, metatable)` makes of a table of type `own`: a table that also has the
    /// fields of `index`, the tables the `__index` of `metatable` gives, as an instance has the
    /// methods of its class. A table with members of its own (`own_table`) is indexed with its
    /// metatables instead, and a declared type is kept.
    fn instance_type(&self, own: Type, index: Type, own_table: bool) -> Type {
        if index.is_unknown() || own_table {
            return own;
        }
        let bare = own.without_nil();
        let parts = match &bare {
            Type::Union(types) => types.as_slice(),
            other => std::slice::from_ref(other),
        };
        if parts.iter().any(|part| matches!(part, Type::Named(..) | Type::Require(_) | Type::Exports(_))) {
            return own;
        }
        // An instance of a `---@class` is that class. The fields set on an instance of a table class
        // are its own, and it falls back on its class for the others.
        let class = match index {
            Type::Named(..) => index,
            other => instances_of(other),
        };
        // An empty table holds nothing of its own.
        if matches!(bare, Type::Table | Type::Unknown) {
            return class;
        }
        // The fields the table holds itself come first, as Lua finds them before those of its
        // metatable. The class goes before the tables the value was an instance of so far, so that
        // the fields set on it are those of its new instances.
        let (held, owners): (Vec<&Type>, Vec<&Type>) =
            parts.iter().partition(|part| !matches!(part, Type::GlobalTable(_)));
        Type::union(held.into_iter().cloned().chain([class]).chain(owners.into_iter().cloned()))
    }

    /// The tables that the `__index` of the metatable passed to a `setmetatable` call as `metatable`
    /// gives.
    fn passed_index(&self, metatable: &Expr) -> Type {
        if let Some(index) = self.passed_indexes.borrow().get(&metatable.span.start) {
            return index.clone();
        }
        // A metatable read from the table it is set on, as in `setmetatable(t, t)`, stops here.
        self.passed_indexes.borrow_mut().insert(metatable.span.start, Type::Unknown);
        let (index, complete) = self.complete(|| self.metatable_index(&self.expr(metatable)));
        if complete {
            self.passed_indexes.borrow_mut().insert(metatable.span.start, index.clone());
        } else {
            self.passed_indexes.borrow_mut().remove(&metatable.span.start);
        }
        index
    }

    /// Whether a metatable that `setmetatable` gives the table `owner` has an `__index` that is no
    /// table, such as a function, which decides at runtime which fields the table has.
    pub fn indexes_at_runtime(&self, owner: &str) -> bool {
        if !self.index.has_metatables(owner) {
            return false;
        }
        self.index.metatables_of(owner, self.ctx.file).into_iter().any(|metatable| {
            let mut found = Vec::new();
            self.guarded(|| self.raw_index(metatable, &mut found, 0));
            let parts = found.into_iter().flat_map(|ty| match ty {
                Type::Union(types) => types,
                other => vec![other],
            });
            let mut parts = parts.filter(|ty| !matches!(ty, Type::Nil));
            parts.any(|ty| match ty {
                Type::GlobalTable(_) | Type::Shape(_) | Type::Require(_) => false,
                Type::Named(name, _) => self.index.class(&name, self.side).is_none(),
                _ => true,
            })
        })
    }

    /// The tables that the `__index` of a metatable of type `metatable` gives, whose fields a table
    /// with that metatable falls back on. A function there decides them at runtime, so it gives none.
    fn metatable_index(&self, metatable: &Type) -> Type {
        let mut found = Vec::new();
        self.guarded(|| self.raw_index(metatable, &mut found, 0));
        let parts = found.into_iter().flat_map(|ty| match ty {
            Type::Union(types) => types,
            other => vec![other],
        });
        Type::union(parts.filter(|ty| match ty {
            Type::GlobalTable(_) | Type::Shape(_) | Type::Require(_) => true,
            Type::Named(name, _) => self.index.class(name, self.side).is_some(),
            _ => false,
        }))
    }

    /// The values of the `__index` field of a metatable of type `metatable`, read from the metatable
    /// itself as Lua reads it, not through a metatable of its own.
    fn raw_index(&self, metatable: &Type, out: &mut Vec<Type>, depth: u32) {
        if depth > 8 {
            return;
        }
        match self.resolve_alias(metatable) {
            Type::Union(types) => {
                for part in types.iter().filter(|t| !matches!(t, Type::Nil)) {
                    self.raw_index(part, out, depth + 1);
                }
            }
            Type::Shape(shape) => out.extend(shape.fields.iter().filter(|f| f.name == "__index").map(|f| f.ty.clone())),
            Type::GlobalTable(owner) => {
                let written = self.written_index(&owner);
                if !written.is_empty() {
                    out.extend(written);
                    return;
                }
                let mut members = Vec::new();
                self.own_members(&owner, Some("__index"), &mut members);
                out.extend(members.into_iter().map(|m| m.ty));
            }
            ty @ Type::Named(..) => out.extend(self.members_matching(&ty, Some("__index")).into_iter().map(|m| m.ty)),
            _ => {}
        }
    }

    /// The values this file writes to the `__index` field of the table `owner`: in the constructor
    /// of a top-level local, or with `table.__index = value`, as `self.__index = self` in a method
    /// does. The index does not hold them yet while the file is indexed for the first time.
    fn written_index(&self, owner: &str) -> Vec<Type> {
        let fields = self.local_table_fields(owner).unwrap_or_default();
        let in_constructor = fields.iter().filter_map(|field| match field {
            TableField::Named { name, value } if name.text == "__index" => Some(value),
            TableField::Keyed { key, value } if key.as_string().is_some_and(|key| key == "__index") => Some(value),
            _ => None,
        });
        let table = Type::GlobalTable(SmolStr::new(owner));
        let assigned =
            self.ctx.index_writes.iter().filter(|(base, _)| self.expr(base) == table).map(|(_, value)| *value);
        in_constructor.chain(assigned).map(|value| self.expr(value)).collect()
    }

    fn unary(&self, op: UnOp, operand: &Expr) -> Type {
        let name = match op {
            UnOp::Not => return Type::Boolean,
            UnOp::Len => "len",
            UnOp::BNot => "bnot",
            UnOp::Neg => "unm",
        };
        let ty = match op {
            UnOp::BNot if !self.index.operator_names().contains(name) => return Type::Integer,
            _ => self.expr(operand).widen(),
        };
        if let Some(result) = self.operator(&ty, name, None) {
            return result;
        }
        match op {
            UnOp::Len => match ty {
                Type::Named(name, _) if name.starts_with("vector") => Type::Number,
                _ => Type::Integer,
            },
            UnOp::Neg => ty,
            _ => Type::Integer,
        }
    }

    /// What the `@operator name` lines of the class of `ty` give for an operand of type `operand`,
    /// or with `None` for an operation without one: the first whose operand is of that type, or
    /// else takes it, and `unknown` when none does. `None` when `ty` is no class or its class
    /// declares no such operator. A class does not take the operators of its parents, as Lua looks
    /// a metamethod up in the metatable itself.
    fn operator(&self, ty: &Type, name: &str, operand: Option<&Type>) -> Option<Type> {
        let Type::Named(class, args) = self.resolve_alias(&ty.without_nil()) else { return None };
        let mut defs = self.index.class_defs(&class);
        defs.retain(|(_, def)| applies_on(def.side, self.side));
        let operators: Vec<&DocOperator> =
            defs.iter().flat_map(|(_, def)| &def.operators).filter(|operator| operator.name == name).collect();
        if operators.is_empty() {
            return None;
        }
        let bindings = class_bindings(&defs, &args);
        let declared = |operator: &DocOperator| operator.operand.as_ref().map(|ty| substitute(ty, &bindings));
        let any = FunType::default();
        let takes = |declared: &Type, given: &Type| match (
            self.value_kinds(&any, declared, true, 0),
            self.value_kinds(&any, given, false, 0),
        ) {
            (Some(taken), Some(given)) => taken & given != 0,
            _ => true,
        };
        let found = match operand {
            Some(given) => operators.iter().find(|operator| declared(operator).as_ref() == Some(given)).or_else(|| {
                operators.iter().find(|operator| declared(operator).is_none_or(|declared| takes(&declared, given)))
            }),
            None => operators.first(),
        };
        Some(found.map_or(Type::Unknown, |operator| substitute(&operator.result, &bindings)))
    }

    /// What `left op right` gives through the `@operator name` lines of the class of `left`, or
    /// else of `right`, as lua-language-server reads `1 + v`. `unknown` when a class declares the
    /// operator for no operand of the other's type, and `None` when neither declares it.
    fn binary_operator(&self, name: &str, left: &Type, right: &Type) -> Option<Type> {
        let from_left = self.operator(left, name, Some(right));
        if from_left.as_ref().is_some_and(|ty| !ty.is_unknown()) {
            return from_left;
        }
        match self.operator(right, name, Some(left)) {
            Some(ty) if !ty.is_unknown() => Some(ty),
            from_right => from_left.or(from_right),
        }
    }

    fn binary(&self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Type {
        // Operands are only inferred for an operator that some class declares.
        let operator = |name: &str| match self.index.operator_names().contains(name) {
            true => self.binary_operator(name, &self.expr(lhs).widen(), &self.expr(rhs).widen()),
            false => None,
        };
        match op {
            BinOp::Concat => operator("concat").unwrap_or(Type::String),
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Type::Boolean,
            // `a and b` is `b` whenever `a` holds a value, and the `nil` or `false` of `a` otherwise,
            // so `cond and 'a'` is a `string|false`. An unknown `b` leaves it unknown.
            BinOp::And => match self.expr(rhs) {
                rhs if rhs.is_unknown() => Type::Unknown,
                rhs => booleans_joined(Type::union(
                    [widened_values(&rhs)].into_iter().chain(self.falsy_parts(&self.expr(lhs))),
                )),
            },
            // `a or b` is what `a` holds when it holds a value, and `b` otherwise, so
            // `cond and 'a' or 'b'` is a `string`.
            BinOp::Or => {
                booleans_joined(Type::union([self.truthy_part(&self.expr(lhs)), widened_values(&self.expr(rhs))]))
            }
            BinOp::BAnd | BinOp::BOr | BinOp::BXor | BinOp::Shl | BinOp::Shr | BinOp::IDiv => {
                let name = match op {
                    BinOp::BAnd => "band",
                    BinOp::BOr => "bor",
                    BinOp::BXor => "bxor",
                    BinOp::Shl => "shl",
                    BinOp::Shr => "shr",
                    _ => "idiv",
                };
                operator(name).unwrap_or(Type::Integer)
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => {
                let (left, right) = (self.expr(lhs).widen(), self.expr(rhs).widen());
                let is_vector = |t: &Type| matches!(t, Type::Named(n, _) if n.starts_with("vector") || n == "quat");
                let name = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::Mul => "mul",
                    BinOp::Div => "div",
                    BinOp::Mod => "mod",
                    _ => "pow",
                };
                // The stubs do not list every operand a vector takes, such as the number of
                // `vector3(1, 2, 3) + 1`.
                match self.binary_operator(name, &left, &right) {
                    Some(ty) if !ty.is_unknown() || !(is_vector(&left) || is_vector(&right)) => return ty,
                    _ => {}
                }
                if is_vector(&left) {
                    left
                } else if is_vector(&right) {
                    right
                } else if left == Type::Integer && right == Type::Integer && !matches!(op, BinOp::Div | BinOp::Pow) {
                    Type::Integer
                } else {
                    Type::Number
                }
            }
        }
    }

    /// The values of `ty` that are false, which `a and b` gives for an `a` of that type: its `nil`
    /// and its `false`, also that of a `boolean`. A type that tells nothing gives none.
    fn falsy_parts(&self, ty: &Type) -> Vec<Type> {
        let expanded = self.expand_aliases(ty, 0);
        let parts = match &expanded {
            Type::Union(parts) => parts.as_slice(),
            one => std::slice::from_ref(one),
        };
        let mut out = Vec::new();
        for part in parts {
            let falsy = match part {
                Type::Nil => Type::Nil,
                Type::Boolean | Type::BooleanLit(false) => Type::BooleanLit(false),
                _ => continue,
            };
            if !out.contains(&falsy) {
                out.push(falsy);
            }
        }
        out
    }

    /// `ty` without the `nil` and `false` it allows, as lua-language-server checks a value read from
    /// a field, which it does not narrow. A type that holds nothing else stays as it is.
    pub fn without_falsy(&self, ty: &Type) -> Type {
        let expanded = self.expand_aliases(ty, 0);
        let Type::Union(parts) = &expanded else { return ty.clone() };
        let is_falsy = |part: &&Type| matches!(part, Type::Nil | Type::BooleanLit(false));
        if !parts.iter().any(|part| is_falsy(&part)) || parts.iter().all(|part| is_falsy(&part)) {
            return ty.clone();
        }
        expanded.rebuilt(parts.iter().filter(|part| !is_falsy(part)).cloned())
    }

    /// What `ty` holds when it holds a true value, which `a or b` gives for an `a` of that type: all
    /// of it but `nil` and `false`, and `true` of a `boolean`.
    fn truthy_part(&self, ty: &Type) -> Type {
        let expanded = self.expand_aliases(ty, 0);
        let parts = match &expanded {
            Type::Union(parts) => parts.as_slice(),
            one => std::slice::from_ref(one),
        };
        // A type with no false value, as an alias of one, is shown as it is written.
        if !parts.iter().any(|part| matches!(part, Type::Nil | Type::Boolean | Type::BooleanLit(false))) {
            return widened_values(ty);
        }
        let truthy = parts.iter().filter_map(|part| match part {
            Type::Nil | Type::BooleanLit(false) => None,
            Type::Boolean => Some(Type::BooleanLit(true)),
            other => Some(widened_values(other)),
        });
        expanded.rebuilt(truthy)
    }

    /// The callee's function type, taking `base:method` lookups into account.
    pub fn callee_fun(&self, base: &Expr, method: Option<&Name>) -> Option<(Arc<FunType>, Option<MemberInfo>)> {
        match method {
            Some(method) => {
                let member = self.member(&self.expr(base), &method.text)?;
                let fun = self.fun_of(&member.ty).or_else(|| self.class_call(&member.ty))?;
                Some((fun, Some(member)))
            }
            None => {
                let ty = self.expr(base);
                Some((self.fun_of(&ty).or_else(|| self.class_call(&ty))?, None))
            }
        }
    }

    /// The function that the first `@operator call(T): R` line of the class `name` makes of its
    /// values, taking a `T` and returning an `R`, as lua-language-server reads it. An `@overload`
    /// of the class comes first.
    fn call_operator(&self, name: &str) -> Option<Arc<FunType>> {
        let defs = self.index.class_defs(name);
        let mut operators =
            defs.iter().filter(|(_, def)| applies_on(def.side, self.side)).flat_map(|(_, def)| &def.operators);
        let operator = operators.find(|operator| operator.name == "call")?;
        let params =
            operator.operand.iter().map(|ty| Param { name: "value".into(), ty: ty.clone(), ..Param::default() });
        Some(Arc::new(FunType {
            params: params.collect(),
            returns: vec![operator.result.clone()],
            ..FunType::default()
        }))
    }

    /// The `@overload` that a value of class `ty` is called with, as `lib.array:new()` calls the
    /// `ArrayConstructor` its `new` field holds, or else the function its `@operator call` makes of it.
    fn class_call(&self, ty: &Type) -> Option<Arc<FunType>> {
        let Type::Named(name, args) = self.resolve_alias(ty) else { return None };
        let call = self.index.class(&name, self.side).and_then(|(_, c)| c.call.clone());
        let call = match call.filter(|f| applies_on(f.side, self.side)) {
            Some(call) => call,
            None => self.call_operator(&name)?,
        };
        let bindings = self.class_bindings_of(&name, &args);
        match bindings.is_empty() {
            true => Some(call),
            false => Some(Arc::new(substitute_fun(&call, &bindings))),
        }
    }

    fn call(&self, base: &Expr, method: Option<&Name>, args: &[Expr]) -> Vec<Type> {
        self.call_values(base, method, args).values
    }

    /// What `call` returns when its function declares it, and `None` when the values are inferred
    /// or `call` is no call.
    pub fn declared_returns(&self, call: &Expr) -> Option<Vec<Type>> {
        self.guarded(|| {
            let returned = match &call.kind {
                ExprKind::Call { callee, args, .. } => self.call_values(callee, None, args),
                ExprKind::MethodCall { base, method, args, .. } => self.call_values(base, Some(method), args),
                _ => return None,
            };
            returned.declared.then_some(returned.values)
        })
    }

    /// What `call` returns when an annotation of its function gives it, also one whose generics the
    /// arguments of the call bind, as `@return T[]` for `T`, and `None` when the values are inferred
    /// or `call` is no call.
    pub fn annotated_returns(&self, call: &Expr) -> Option<Vec<Type>> {
        self.guarded(|| {
            let returned = match &call.kind {
                ExprKind::Call { callee, args, .. } => self.call_values(callee, None, args),
                ExprKind::MethodCall { base, method, args, .. } => self.call_values(base, Some(method), args),
                _ => return None,
            };
            returned.annotated.then_some(returned.values)
        })
    }

    /// The types the generics of `fun` take from `types`, those of the arguments `args` of a call,
    /// as a call binds them: the first argument that decides a generic decides it, a function
    /// literal decides none, and a generic that no argument decides is unknown. `table.insert(list,
    /// value)` binds the `T` of `fun(list: T[], value: T)` to the `number` of a `number[]` list.
    pub fn generics_bound_by(
        &self,
        fun: &FunType,
        args: &[Expr],
        via_method: bool,
        types: &[Type],
    ) -> Vec<(SmolStr, Type)> {
        let typed = |arg: &Expr| {
            let index = args.iter().position(|other| std::ptr::eq(other, arg));
            index.and_then(|index| types.get(index)).cloned().unwrap_or_default()
        };
        self.bind_generics(fun, &CallArgs::typed(args, typed), via_method, false)
    }

    /// What `call` returns when its function is a generic that declares its values with `@return`,
    /// with the generics bound from the types `declared` gives the arguments, as TypeScript binds
    /// them: `first(names)` gives a `string?` for `fun(list: V[]): V?` and a `names` declared as
    /// `string[]`. A generic they leave unbound is unknown. `None` when the function is no generic,
    /// infers its values, or is one whose call `call_values` reads in its own way, as
    /// `setmetatable`, or when `call` is no call.
    pub fn declared_generic_returns(&self, call: &Expr, declared: impl Fn(&Expr) -> Type) -> Option<Vec<Type>> {
        self.guarded(|| {
            let (base, method, exprs) = match &call.kind {
                ExprKind::Call { callee, args, .. } => (callee, None, args),
                ExprKind::MethodCall { base, method, args, .. } => (base, Some(method), args),
                _ => return None,
            };
            if method.is_none()
                && matches!(
                    base.dotted_path().as_deref(),
                    Some("require" | "lib.require" | "lib.load" | "setmetatable")
                )
            {
                return None;
            }
            let via_method = method.is_some();
            let args = CallArgs::new(exprs);
            let fun = match self.sided_definition(base, method, &args) {
                Some(fun) => fun?,
                None => self.callee_fun(base, method)?.0,
            };
            let fun = self.signature_for(&fun, &args, via_method, base.span.start);
            let awaits = Wrapper::of(&fun, via_method).is_some_and(|w| w.tag.role == CallbackRole::Await);
            if fun.generics.is_empty() || fun.returns_inferred || fun.returns_nothing || awaits {
                return None;
            }
            let generics = self.bind_generics(&fun, &CallArgs::typed(exprs, &declared), via_method, false);
            let values = fun.returns.iter().map(|ret| substitute(ret, &generics)).collect();
            Some(self.asserted(base, method, values, || exprs.first().map(&declared)))
        })
    }

    /// `values` of a call of `base`, or its `method`, where an `assert(v)` gives its `v`, of the type
    /// `first` gives it, without the `nil` and `false` it raises an error for, as lua-language-server
    /// reads it. A generic would widen the `false` of a `string|false` to a `boolean`.
    fn asserted(
        &self,
        base: &Expr,
        method: Option<&Name>,
        mut values: Vec<Type>,
        first: impl FnOnce() -> Option<Type>,
    ) -> Vec<Type> {
        let ExprKind::Name(name) = &base.unparen().kind else { return values };
        let is_global = !matches!(self.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Local(_)));
        if method.is_some() || name.text != "assert" || !is_global {
            return values;
        }
        if let (Some(value), Some(first)) = (values.first_mut(), first()) {
            *value = self.without_falsy(&first);
        }
        values
    }

    /// Whether `call` runs a function that returns nothing, so what it gives is surely `nil`.
    pub fn returns_nothing(&self, call: &Expr) -> bool {
        self.guarded(|| match &call.unparen().kind {
            ExprKind::Call { callee, args, .. } => self.call_values(callee, None, args).nothing,
            ExprKind::MethodCall { base, method, args, .. } => self.call_values(base, Some(method), args).nothing,
            _ => false,
        })
    }

    /// What `call` returns when its function infers it from the `return`s of its body, with the `nil`
    /// of a body that runs past its end only when `past_end` asks for it, and `None` when the values
    /// are declared or `call` is no call.
    pub fn inferred_returns_of(&self, call: &Expr, past_end: bool) -> Option<Vec<Type>> {
        self.guarded(|| {
            let returned = match &call.kind {
                ExprKind::Call { callee, args, .. } => self.call_values(callee, None, args),
                ExprKind::MethodCall { base, method, args, .. } => self.call_values(base, Some(method), args),
                _ => return None,
            };
            let Returned { values, inferred, .. } = returned;
            inferred.map(|returned| if past_end { values } else { returned })
        })
    }

    /// How many values `call` gives, when each signature its function may use, and each set of
    /// values it lists, gives the same number of them and none ends with `...T`: the `1` of
    /// `tonumber(x)`. `None` when that is not known, or is none.
    pub fn value_count(&self, call: &Expr) -> Option<usize> {
        let (base, method, args) = match &call.kind {
            ExprKind::Call { callee, args, .. } => (&**callee, None, args),
            ExprKind::MethodCall { base, method, args, .. } => (&**base, Some(method), args),
            _ => return None,
        };
        if method.is_none() && matches!(base.dotted_path().as_deref(), Some("tonumber" | "tostring")) {
            return Some(1);
        }
        let fun = match self.sided_definition(base, method, &CallArgs::new(args)) {
            Some(fun) => fun?,
            None => self.callee_fun(base, method)?.0,
        };
        // An `await` wrapper returns what the handler it reaches returns.
        if Wrapper::of(&fun, method.is_some()).is_some_and(|wrapper| wrapper.tag.role == CallbackRole::Await) {
            return None;
        }
        let signatures = self.signatures_at(&fun, base.span.start);
        let mut counts = signatures
            .iter()
            .flat_map(|signature| std::iter::once(&signature.returns).chain(&signature.return_sets))
            .map(|values| match values.last() {
                Some(Type::Variadic(_)) => None,
                _ => Some(values.len()),
            });
        let first = counts.next()??;
        (first > 0 && counts.all(|count| count == Some(first))).then_some(first)
    }

    /// What calling `base`, or its `method`, with `args` returns.
    fn call_values(&self, base: &Expr, method: Option<&Name>, args: &[Expr]) -> Returned {
        let only = |ty: Type, declared: bool| Returned {
            values: vec![ty],
            sets: Vec::new(),
            declared,
            nothing: false,
            inferred: None,
            annotated: declared,
        };
        if method.is_none() {
            match (base.dotted_path().as_deref(), args.first()) {
                (Some("require" | "lib.require" | "lib.load"), Some(arg)) => {
                    if let Some(path) = arg.as_string() {
                        // `require 'glm'` returns the built-in library, not a file of the resource.
                        if path == "glm" {
                            return only(self.global_type("glm"), false);
                        }
                        return only(Type::Require(path.clone()), false);
                    }
                }
                (Some("setmetatable"), Some(arg)) => {
                    let own = self.expr(arg);
                    let ty = match args.get(1) {
                        Some(metatable) => {
                            let own_table = self.names_own_table(arg, &own);
                            self.instance_type(own, self.passed_index(metatable), own_table)
                        }
                        None => own,
                    };
                    return only(ty, false);
                }
                (Some("tostring"), _) => return only(Type::String, true),
                // `tonumber(n)` gives back a number it is given, and `nil` only for another value
                // or with a base, which it reads a string in.
                (Some("tonumber"), Some(arg)) if args.len() == 1 => {
                    let ty = match self.expr(arg).widen() {
                        ty @ (Type::Number | Type::Integer) => ty,
                        _ => Type::Number.optional(),
                    };
                    return only(ty, true);
                }
                (Some("tonumber"), _) => return only(Type::Number.optional(), true),
                _ => {}
            }
        }
        if !self.typing.borrow().is_empty() && self.calls_itself(base, method) {
            return only(Type::Unknown, false);
        }
        let args = CallArgs::new(args);
        let fun = match self.sided_definition(base, method, &args) {
            Some(Some(fun)) => fun,
            Some(None) => return Default::default(),
            None => match self.callee_fun(base, method) {
                Some((fun, _)) => fun,
                None => return Default::default(),
            },
        };
        let fun = self.signature_for(&fun, &args, method.is_some(), base.span.start);
        // `AwaitServerCallback('name', ...)` returns what the handler of `name` returns.
        if let Some(wrapper) = Wrapper::of(&fun, method.is_some()).filter(|w| w.tag.role == CallbackRole::Await) {
            if let Some(handler) = self.wrapper_handler(&wrapper, args.exprs, base.span.start) {
                if !handler.returns.is_empty() {
                    let inferred = handler.explicit_returns.as_ref().unwrap_or(&handler.returns);
                    return Returned {
                        values: handler.returns.clone(),
                        sets: handler.return_sets.clone(),
                        declared: !handler.returns_inferred,
                        nothing: false,
                        inferred: handler.returns_inferred.then(|| inferred.clone()),
                        annotated: !handler.returns_inferred,
                    };
                }
            }
        }
        // A function that returns nothing gives `nil`, which no `@return` declares.
        if fun.returns_nothing {
            return Returned { nothing: true, ..only(Type::Nil, false) };
        }
        let generics = self.bind_generics(&fun, &args, method.is_some(), true);
        let bound = |types: &Vec<Type>| types.iter().map(|ret| substitute(ret, &generics)).collect();
        Returned {
            values: self
                .asserted(base, method, bound(&fun.returns), || args.exprs.first().map(|_| args.ty(self, 0).clone())),
            sets: fun.return_sets.iter().map(bound).collect(),
            declared: !fun.returns_inferred && fun.generics.is_empty(),
            nothing: false,
            inferred: fun.returns_inferred.then(|| bound(fun.explicit_returns.as_ref().unwrap_or(&fun.returns))),
            annotated: !fun.returns_inferred,
        }
    }

    /// The definition that a call of the global function `base` uses when client and server files
    /// define it differently, as a `shared_script` sees two `GetJob`s: the nearest one of each side
    /// the call runs on, if they are the same one and its arguments fit it. `Some(None)` when no
    /// single definition is left or the arguments do not fit it, and `None` when the sides do not
    /// split the global.
    fn sided_definition(&self, base: &Expr, method: Option<&Name>, args: &CallArgs) -> Option<Option<Arc<FunType>>> {
        let (None, ExprKind::Name(name)) = (method, &base.kind) else { return None };
        if !matches!(self.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_))) {
            return None;
        }
        let side_of = |file: FileId| self.index.file(file).and_then(|f| f.side);
        let defined: Vec<(Option<Side>, Distance, &Arc<FunType>)> = self
            .index
            .globals_named(&name.text, self.ctx.file)
            .into_iter()
            .filter_map(|(file, symbol)| {
                Some((side_of(file), self.index.distance(self.ctx.file, file), symbol.ty.as_fun()?))
            })
            .collect();
        let defined_on = |side| defined.iter().any(|(on, ..)| *on == Some(side));
        if !defined_on(Side::Client) || !defined_on(Side::Server) {
            return None;
        }
        let sides = match self.side_at(base.span.start) {
            Some(Side::Client) => &[Side::Client][..],
            Some(Side::Server) => &[Side::Server],
            _ => &[Side::Client, Side::Server],
        };
        let mut left: Vec<&Arc<FunType>> = Vec::new();
        for side in sides {
            let applying = || defined.iter().filter(|(on, ..)| applies_on(*on, Some(*side)));
            let nearest = applying().map(|(_, distance, _)| *distance).min();
            for (_, _, fun) in applying().filter(|(_, distance, _)| Some(*distance) == nearest) {
                if !left.contains(fun) {
                    left.push(fun);
                }
            }
        }
        let fits = |fun: &Arc<FunType>| {
            let signature = self.signature_for(fun, args, false, base.span.start);
            self.fit(&signature, args, false) != Fit::No
        };
        Some(match left[..] {
            [fun] if fits(fun) => Some(fun.clone()),
            _ => None,
        })
    }

    /// Binds the generics of `fun` from the arguments of a call. Function literals go last, and only
    /// `with_callbacks`: their parameters are typed from what the other arguments bound, and their
    /// returns bind the rest, as `RV` and `RK` in `fun(value: V, key: K): RV, RK`.
    fn bind_generics(
        &self,
        fun: &FunType,
        args: &CallArgs,
        via_method: bool,
        with_callbacks: bool,
    ) -> Vec<(SmolStr, Type)> {
        let mut bound = Vec::new();
        let (skip_params, skip_args) = fun.call_offsets(via_method);
        let bindable: Vec<(&Param, usize)> = fun
            .params
            .iter()
            .skip(skip_params)
            .zip(skip_args..args.exprs.len())
            .filter(|(param, _)| self.can_bind(fun, &param.ty))
            .collect();
        let is_callback = |i: &usize| matches!(args.exprs[*i].unparen().kind, ExprKind::Function(_));
        for (param, i) in bindable.iter().filter(|(_, i)| !is_callback(i)) {
            match self.named_elements(&param.ty, &args.exprs[*i]) {
                Some(names) => self.unify(fun, &param.ty, &names, false, &mut bound, 0),
                None => self.unify(fun, &param.ty, args.ty(self, *i), false, &mut bound, 0),
            }
        }
        if with_callbacks {
            for (param, i) in bindable.iter().filter(|(_, i)| is_callback(i)) {
                self.unify(fun, &param.ty, args.ty(self, *i), false, &mut bound, 0);
            }
        }
        // A declared generic that no argument decides is unknown, not a type named `RV`.
        for name in &fun.generics {
            if !bound.iter().any(|(n, _)| n == name) {
                bound.push((name.clone(), Type::Unknown));
            }
        }
        bound
    }

    /// Matches a parameter type against the type of its argument, binding the generics in it. A
    /// generic takes the argument widened, as a variable initialised with it would be, unless it is
    /// `returned` by a function passed for a `fun(...)` parameter. `` `T` `` takes the class or
    /// alias that a string literal names.
    fn unify(
        &self,
        fun: &FunType,
        param: &Type,
        arg: &Type,
        returned: bool,
        bound: &mut Vec<(SmolStr, Type)>,
        depth: u32,
    ) {
        if depth > 8 || arg.is_unknown() {
            return;
        }
        let mut bind = |name: &str, ty: Type| {
            if !bound.iter().any(|(n, _)| n == name) {
                bound.push((SmolStr::new(name), ty));
            }
        };
        match param {
            Type::Named(name, args) if args.is_empty() && self.is_generic(fun, name) => {
                bind(name, if returned { arg.clone() } else { arg.widen() });
            }
            Type::NameOf(name) if self.is_generic(fun, name) => match arg {
                Type::StringLit(named) => bind(name, Type::named(named)),
                // `{ "Player", "Car" }` for `` `T`[] `` names either class.
                Type::Union(parts) if parts.iter().all(|part| matches!(part, Type::StringLit(_))) => {
                    let named = parts.iter().filter_map(|part| match part {
                        Type::StringLit(named) => Some(Type::named(named)),
                        _ => None,
                    });
                    bind(name, Type::union(named));
                }
                _ => {}
            },
            // `List<T>` takes the `string` of a `List<string>`, and `Result<T>` of
            // `---@alias Result<T> T|nil` what `T|nil` takes, unless it is given a `Result<string>`.
            Type::Named(name, params) if !params.is_empty() => {
                let given = match arg.without_nil() {
                    Type::Named(given, args) if given == *name => Type::Named(given, args),
                    other => self.resolve_alias(&other),
                };
                match given {
                    Type::Named(given, args) if given == *name => {
                        for (param, arg) in params.iter().zip(&args) {
                            self.unify(fun, param, arg, returned, bound, depth + 1);
                        }
                    }
                    _ => {
                        let expanded = self.resolve_alias(param);
                        if expanded != *param {
                            self.unify(fun, &expanded, arg, returned, bound, depth + 1);
                        }
                    }
                }
            }
            Type::Array(inner) => {
                self.unify(fun, inner, &self.key_value_types(arg, true).1, returned, bound, depth + 1);
            }
            Type::Map(key, value) => {
                let (arg_key, arg_value) = self.key_value_types(arg, false);
                self.unify(fun, key, &arg_key, returned, bound, depth + 1);
                self.unify(fun, value, &arg_value, returned, bound, depth + 1);
            }
            Type::Union(types) => {
                for part in types.iter().filter(|t| !matches!(t, Type::Nil)) {
                    self.unify(fun, part, &arg.without_nil(), returned, bound, depth + 1);
                }
            }
            Type::Fun(expected) => {
                if let Some(given) = arg.as_fun() {
                    for (want, got) in expected.returns.iter().zip(&given.returns) {
                        self.unify(fun, want, got, true, bound, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }

    /// The type of a table constructor passed for `` `T`[] ``, whose strings name the classes `T`
    /// stands for: an array of those strings as written, which a table built elsewhere widens to
    /// `string`.
    fn named_elements(&self, param: &Type, arg: &Expr) -> Option<Type> {
        let Type::Array(inner) = param.without_nil() else { return None };
        let fields = self.constructor_of(arg)?;
        if !matches!(*inner, Type::NameOf(_)) || fields.is_empty() {
            return None;
        }
        let elements = table_elements(fields);
        Some(Type::Array(Box::new(Type::union(elements.array.iter().map(|value| self.expr(value))))))
    }

    /// The table constructor `expr` is, or that the local it names is declared with when it is
    /// never assigned again, as `names` of `local names = { 'Player' }`.
    fn constructor_of<'s>(&'s self, expr: &'s Expr) -> Option<&'s [TableField]> {
        match &expr.unparen().kind {
            ExprKind::Table(fields) => Some(fields),
            ExprKind::Name(name) => {
                let Some(Resolved::Local(id)) = self.ctx.resolution.resolve_at(name.span.start) else { return None };
                let local = self.ctx.resolution.local(id);
                if local.refs.iter().any(|r| r.write) {
                    return None;
                }
                let Some(Decl::Local { stmt, index }) = self.ctx.decl(local.decl.start) else { return None };
                let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return None };
                match &exprs.get(*index)?.unparen().kind {
                    ExprKind::Table(fields) => Some(fields),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Whether an argument passed for `param` can bind a generic of `fun`, as far as `unify` looks.
    fn can_bind(&self, fun: &FunType, param: &Type) -> bool {
        match param {
            Type::Named(name, args) if args.is_empty() => self.is_generic(fun, name),
            Type::Named(_, args) => args.iter().any(|arg| self.can_bind(fun, arg)),
            Type::NameOf(name) => self.is_generic(fun, name),
            Type::Array(inner) => self.can_bind(fun, inner),
            Type::Map(key, value) => self.can_bind(fun, key) || self.can_bind(fun, value),
            Type::Union(types) => types.iter().any(|t| self.can_bind(fun, t)),
            Type::Fun(expected) => expected.returns.iter().any(|t| self.can_bind(fun, t)),
            _ => false,
        }
    }

    /// The signature a call uses: the declared one, or else the `@overload` that fits best, so
    /// `fun(x, y, z): vector3` wins over a `vec(...)` that only takes three values through `...`, and
    /// `fun(action: "keyPressed")` wins over `fun(action: string)` for `"keyPressed"`, as well as over
    /// an `action: Actions` whose alias lists `"keyPressed"` among others. Ties go to the declared
    /// signature, then to the earliest overload. An `@overload (server)` only counts for a call at
    /// `at` that runs on the server.
    fn signature_for(&self, fun: &Arc<FunType>, args: &CallArgs, via_method: bool, at: u32) -> Arc<FunType> {
        if fun.overloads.is_empty() {
            return fun.clone();
        }
        let signatures = self.signatures_at(fun, at);
        signatures[self.best_fit(&signatures, args, via_method)].clone()
    }

    /// The signature a call picks, as `signature_for` does while inferring it.
    pub fn call_signature(&self, fun: &Arc<FunType>, args: &[Expr], via_method: bool, at: u32) -> Arc<FunType> {
        self.signature_for(fun, &CallArgs::new(args), via_method, at)
    }

    /// The signatures a call at `at` can use, and the index of the one its arguments pick.
    pub fn call_signatures(
        &self,
        fun: &Arc<FunType>,
        args: &[Expr],
        via_method: bool,
        at: u32,
    ) -> (Vec<Arc<FunType>>, usize) {
        let signatures = self.signatures_at(fun, at);
        let best = if signatures.len() > 1 { self.best_fit(&signatures, &CallArgs::new(args), via_method) } else { 0 };
        (signatures, best)
    }

    /// The signatures a call at `at` that is still being written can use, given the `args` before
    /// the cursor, each with whether it fits them best: `OnAction("playerUnloaded", ` fits the
    /// `fun(action: "playerUnloaded", handler: fun(source: number))` overload best, although its
    /// handler is not passed yet. When no signature fits, the declared one is the best there is.
    pub fn open_call_signatures(
        &self,
        fun: &Arc<FunType>,
        args: &[Expr],
        via_method: bool,
        at: u32,
    ) -> Vec<(Arc<FunType>, bool)> {
        let call = CallArgs { open: true, ..CallArgs::new(args) };
        let fits: Vec<(Arc<FunType>, Fit)> = self
            .signatures_at(fun, at)
            .into_iter()
            .map(|signature| {
                let fit = self.fit(&signature, &call, via_method);
                (signature, fit)
            })
            .collect();
        let best = fits.iter().map(|(_, fit)| *fit).max().unwrap_or(Fit::No);
        if best == Fit::No {
            return vec![(fun.clone(), true)];
        }
        fits.into_iter().filter(|(_, fit)| *fit != Fit::No).map(|(signature, fit)| (signature, fit == best)).collect()
    }

    /// The values a type lists, through aliases and unions: `"a"` and `"b"` of `"a"|"b"|string`, `1`
    /// and `2` of `1|2`, and `true` and `false` of `boolean`. A type that lists any also lists the
    /// `nil` it allows, last; alone, `nil` tells that a value may be missing, not what it can be.
    pub fn listed_literals(&self, ty: &Type) -> Vec<Type> {
        let mut out = Vec::new();
        self.collect_listed_literals(ty, 0, &mut out);
        let listed = out.len();
        out.retain(|value| !matches!(value, Type::Nil));
        if listed > out.len() && !out.is_empty() {
            out.push(Type::Nil);
        }
        out
    }

    fn collect_listed_literals(&self, ty: &Type, depth: u32, out: &mut Vec<Type>) {
        if depth > 8 {
            return;
        }
        match ty {
            Type::StringLit(_) | Type::IntLit(_) | Type::BooleanLit(_) | Type::Nil if !out.contains(ty) => {
                out.push(ty.clone())
            }
            Type::Boolean => {
                for value in [Type::BooleanLit(true), Type::BooleanLit(false)] {
                    if !out.contains(&value) {
                        out.push(value);
                    }
                }
            }
            Type::Named(..) => match self.resolve_alias(ty) {
                Type::Named(..) => {}
                resolved => self.collect_listed_literals(&resolved, depth + 1, out),
            },
            Type::Union(types) => {
                for part in types {
                    self.collect_listed_literals(part, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    /// The one literal a parameter takes besides `nil`, as `"keyPressed"` for `action: "keyPressed"`.
    pub fn pinned_literal(&self, param: &Type) -> Option<Type> {
        self.sole_literal(param, 0)
    }

    /// The declared signature of `fun`, then the `@overload`s that apply to the code at `at`.
    fn signatures_at(&self, fun: &Arc<FunType>, at: u32) -> Vec<Arc<FunType>> {
        let side = self.side_at(at);
        let overloads = fun.overloads.iter().filter(|overload| applies_on(overload.side, side));
        std::iter::once(fun).chain(overloads).cloned().collect()
    }

    /// The index of the signature a call fits best, the earliest of those that fit equally well.
    fn best_fit(&self, signatures: &[Arc<FunType>], args: &CallArgs, via_method: bool) -> usize {
        let mut best = (Fit::No, 0);
        for (i, signature) in signatures.iter().enumerate() {
            let fit = self.fit(signature, args, via_method);
            if i == 0 || fit > best.0 {
                best = (fit, i);
            }
        }
        best.1
    }

    /// The handler registered under the name a call at `at` passes to an `await` or `trigger` wrapper.
    fn wrapper_handler(&self, wrapper: &Wrapper, args: &[Expr], at: u32) -> Option<Arc<FunType>> {
        let name = args.get(wrapper.arg(wrapper.name))?.as_string()?;
        let target = callback_wrappers::target_of(self.side_at(at));
        let (_, event) = callback_wrappers::handler(self.index, &wrapper.family(), name, target)?;
        event.handler.clone()
    }

    fn fit(&self, fun: &FunType, call: &CallArgs, via_method: bool) -> Fit {
        let (skip_params, skip_args) = fun.call_offsets(via_method);
        let params = fun.params.get(skip_params..).unwrap_or_default();
        let args = call.exprs.get(skip_args..).unwrap_or_default();
        let (fixed, variadic) = match params.split_last() {
            Some((last, rest)) if last.name == "..." => (rest, true),
            _ => (params, false),
        };
        // A call ending in `f()` or `...` passes any number of values in its last argument.
        let open_ended = args.last().is_some_and(Expr::is_multi_value);
        let through_vararg = args.len() - usize::from(open_ended) > fixed.len();
        if through_vararg && !variadic {
            return Fit::No;
        }
        let missing_required =
            fixed.iter().skip(args.len()).any(|p| self.param_kinds(fun, p).is_some_and(|kinds| kinds & kind::NIL == 0));
        if missing_required && !open_ended && !call.open {
            return Fit::No;
        }
        let mut literals = Literals::default();
        for (i, (param, arg)) in fixed.iter().zip(args).enumerate() {
            // Function literals and table constructors are not inferred here: the parameters of the
            // functions in them may be typed from this very call.
            let (given, given_kinds) = match arg.unparen().kind {
                ExprKind::Function(_) => (None, Some(kind::FUNCTION)),
                ExprKind::Table(_) => (None, Some(kind::TABLE)),
                _ => {
                    let given = call.ty(self, skip_args + i);
                    (Some(given), self.value_kinds(fun, given, false, 0))
                }
            };
            if let (Some(wanted), Some(given_kinds)) = (self.param_kinds(fun, param), given_kinds) {
                if wanted & given_kinds == 0 {
                    return Fit::No;
                }
            }
            let Some(given) = given else { continue };
            match self.literal_fit(fun, &param.ty, given, 0) {
                Some(true) => {
                    literals.listed += 1;
                    literals.pinned += usize::from(self.sole_literal(&param.ty, 0).as_ref() == Some(given));
                }
                Some(false) => return Fit::No,
                None => {}
            }
        }
        if through_vararg {
            Fit::ThroughVararg(literals)
        } else {
            Fit::Exact(literals)
        }
    }

    /// Whether a literal argument is one of the values its parameter lists, such as `"keyPressed"`
    /// for `action: "keyPressed"|"keyReleased"`. `None` when the argument is no literal, or the
    /// parameter also takes other values of its kind, as `string` or `any` does.
    fn literal_fit(&self, fun: &FunType, param: &Type, arg: &Type, depth: u32) -> Option<bool> {
        if depth > 8 || !matches!(arg, Type::StringLit(_) | Type::IntLit(_) | Type::BooleanLit(_)) {
            return None;
        }
        let resolved;
        let param = match param {
            Type::Named(..) => {
                resolved = self.resolve_alias(param);
                &resolved
            }
            _ => param,
        };
        match param {
            Type::StringLit(_) | Type::IntLit(_) | Type::BooleanLit(_) => Some(param == arg),
            Type::Union(types) => {
                let mut fit = Some(false);
                for part in types {
                    match self.literal_fit(fun, part, arg, depth + 1) {
                        Some(true) => return Some(true),
                        Some(false) => {}
                        None => fit = None,
                    }
                }
                fit
            }
            // The `nil` of `"all"|nil` takes a string no more than `"all"` does.
            _ => {
                let wanted = self.value_kinds(fun, param, true, depth + 1)?;
                let given = self.value_kinds(fun, arg, false, 0)?;
                (wanted & given == 0).then_some(false)
            }
        }
    }

    /// The one literal a parameter takes besides `nil`: `"keyPressed"` for `action: "keyPressed"`,
    /// `key?: "E"` or an alias of either.
    fn sole_literal(&self, param: &Type, depth: u32) -> Option<Type> {
        if depth > 8 {
            return None;
        }
        match param {
            Type::StringLit(_) | Type::IntLit(_) | Type::BooleanLit(_) => Some(param.clone()),
            Type::Named(..) => match self.resolve_alias(param) {
                Type::Named(..) => None,
                resolved => self.sole_literal(&resolved, depth + 1),
            },
            Type::Union(types) => {
                let mut parts = types.iter().filter(|t| !matches!(t, Type::Nil));
                match (parts.next(), parts.next()) {
                    (Some(only), None) => self.sole_literal(only, depth + 1),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The kinds of value a parameter takes: `id? integer` is stored as `integer`, and also takes `nil`.
    fn param_kinds(&self, fun: &FunType, param: &Param) -> Option<u8> {
        self.value_kinds(fun, &param.ty, true, 0).map(|kinds| if param.optional { kinds | kind::NIL } else { kinds })
    }

    /// The kinds of Lua value `ty` allows, as a set of `kind` bits, or `None` when it allows any.
    /// `param` tells that `ty` is the type of a parameter.
    fn value_kinds(&self, fun: &FunType, ty: &Type, param: bool, depth: u32) -> Option<u8> {
        if depth > 8 {
            return None;
        }
        Some(match ty {
            Type::Unknown | Type::Any => return None,
            Type::Nil => kind::NIL,
            Type::Boolean | Type::BooleanLit(_) => kind::BOOLEAN,
            Type::Number | Type::Integer | Type::IntLit(_) | Type::Handle(_) => kind::NUMBER,
            Type::String | Type::StringLit(_) => kind::STRING,
            // A parameter `` `T` `` takes the name of a class. Anywhere else it is that class,
            // which may be anything.
            Type::NameOf(_) if param => kind::STRING,
            Type::NameOf(_) => return None,
            Type::Table
            | Type::Array(_)
            | Type::Map(..)
            | Type::Tuple(_)
            | Type::Shape(_)
            | Type::GlobalTable(_)
            | Type::Exports(_)
            | Type::Require(_) => kind::TABLE,
            Type::Function | Type::Fun(_) => kind::FUNCTION,
            Type::Thread | Type::Userdata => kind::OTHER,
            Type::Variadic(inner) => return self.value_kinds(fun, inner, param, depth + 1),
            Type::Named(name, _) if self.is_generic(fun, name) || NATIVE_HANDLE_TYPES.contains(&name.as_str()) => {
                return None
            }
            // Classes describe tables, and also userdata such as `vector3`.
            Type::Named(name, _) if self.index.class(name, self.side).is_some() => kind::TABLE | kind::OTHER,
            Type::Named(..) => match self.resolve_alias(ty) {
                Type::Named(..) => return None,
                resolved => return self.value_kinds(fun, &resolved, param, depth + 1),
            },
            Type::Union(types) => {
                let mut kinds = 0;
                for part in types {
                    kinds |= self.value_kinds(fun, part, param, depth + 1)?;
                }
                kinds
            }
        })
    }

    /// A name `fun` declares with `@generic`, or a short one such as `T` that is neither a class nor
    /// an alias.
    fn is_generic(&self, fun: &FunType, name: &str) -> bool {
        fun.generics.iter().any(|g| g == name)
            || (name.len() <= 2
                && self.index.class(name, self.side).is_none()
                && self.index.alias(name, self.side).is_none())
    }

    /// Builds the type of a function literal from its doc comment, inferring returns when undocumented.
    pub fn fun_type(&self, func: &FuncBody, doc_anchor: Option<u32>, is_method: bool) -> FunType {
        let names: Vec<SmolStr> = func.params.iter().map(|p| p.text.clone()).collect();
        let doc = doc_anchor.map(|anchor| self.ctx.doc_at(anchor));
        let mut fun = match &doc {
            Some(doc) => doc.fun_type(&names, func.vararg.is_some(), is_method),
            None => DocGroup::default().fun_type(&names, func.vararg.is_some(), is_method),
        };
        if let Some(anchor) = doc_anchor.filter(|_| fun.mentions_self()) {
            fun = fun.with_self(&self.doc_self_at(anchor, func));
        }
        if fun.returns.is_empty() {
            let (returns, sets, explicit) = self.inferred_returns(&func.body);
            if returns.iter().any(|t| !t.is_unknown()) {
                fun.returns = returns;
                fun.return_sets = sets;
                fun.returns_inferred = true;
                fun.explicit_returns = explicit;
            }
            // `function Framework.GetJob() return end`, or a stub with no `@return`, returns nothing.
            fun.returns_nothing =
                fun.returns.is_empty() && return_stmts(&func.body).iter().all(|(_, exprs)| exprs.is_empty());
        }
        fun
    }

    /// What an undocumented function returns: at each position, the union of what every `return`
    /// passes there. A `return` with fewer values, and running past the end of the body, give `nil`.
    /// With them come the sets of values its `return`s pass, when they differ and hold several
    /// values, as `(false)` and `(string, string)` do, and what they pass as they are written, as
    /// `FunType::explicit_returns` reads them.
    fn inferred_returns(&self, body: &Block) -> (Vec<Type>, Vec<Vec<Type>>, Option<Vec<Type>>) {
        let exits: Vec<&[Expr]> = return_stmts(body).into_iter().map(|(_, exprs)| exprs).collect();
        if exits.is_empty() {
            return Default::default();
        }
        let merged = |lists: &[Vec<Type>]| {
            let width = lists.iter().map(Vec::len).max().unwrap_or(0);
            let at = |i: usize| lists.iter().map(|values| values.get(i).cloned().unwrap_or(Type::Nil)).collect();
            (0..width).map(|i| merge_values(at(i)).widen_returned()).collect::<Vec<Type>>()
        };
        let (mut lists, written): (Vec<Vec<Type>>, Vec<Vec<Type>>) =
            exits.iter().map(|exprs| self.return_values(exprs)).unzip();
        if !always_exits(body) {
            lists.push(Vec::new());
        }
        let returns = merged(&lists);
        let mut sets: Vec<Vec<Type>> = Vec::new();
        for list in &lists {
            if !sets.contains(list) {
                sets.push(list.clone());
            }
        }
        let linked = returns.len() > 1 && (2..=MAX_RETURN_SETS).contains(&sets.len());
        (returns, if linked { sets } else { Vec::new() }, Some(merged(&written)))
    }

    /// The values one `return` passes, the last of them spread when it is a call or `...`, and the
    /// same values as it writes them: the `nil` and `false` it writes out, and what a call at its end
    /// declares or returns as written, while the `nil` or `false` that other values may hold, as
    /// `t[k]` or a local may, is left out. A plain `true` or `false` stays, so that the sets of values
    /// tell `return false` from `return name`.
    fn return_values(&self, exprs: &[Expr]) -> (Vec<Type>, Vec<Type>) {
        let returned = |ty: Type| match ty {
            Type::BooleanLit(_) => ty,
            other => other.widen_returned(),
        };
        let (mut values, mut written) = (Vec::new(), Vec::new());
        for (i, expr) in exprs.iter().enumerate() {
            let (given, as_written) = match &expr.unparen().kind {
                ExprKind::Nil => (vec![Type::Nil], vec![Type::Nil]),
                ExprKind::False => (vec![Type::BooleanLit(false)], vec![Type::BooleanLit(false)]),
                _ if i + 1 == exprs.len() => self.spread_values(expr),
                _ => {
                    let ty = self.expr(expr);
                    (vec![ty.clone()], vec![without_missing(ty)])
                }
            };
            values.extend(given.into_iter().map(returned));
            written.extend(as_written.into_iter().map(returned));
        }
        (values, written)
    }

    /// What `expr`, the last value of a `return`, gives, and the same values as `return_values`
    /// reads them as written: those that a call declares, or that its function returns as written.
    fn spread_values(&self, expr: &Expr) -> (Vec<Type>, Vec<Type>) {
        let called = self.guarded(|| match &expr.kind {
            ExprKind::Call { callee, args, .. } => Some(self.call_values(callee, None, args)),
            ExprKind::MethodCall { base, method, args, .. } => Some(self.call_values(base, Some(method), args)),
            _ => None,
        });
        match called.filter(|_| self.cast_after(expr.span.end).is_none()) {
            // `return print('none')` passes the `nil` of a function that returns nothing, as written.
            Some(Returned { nothing: true, .. }) => (vec![Type::Nil], vec![Type::Nil]),
            Some(Returned { values, declared: true, .. }) => (values.clone(), values),
            Some(Returned { values, inferred: Some(inferred), .. }) => (values, inferred),
            Some(Returned { values, .. }) => {
                let written = values.iter().cloned().map(without_missing).collect();
                (values, written)
            }
            None => {
                let values = self.expr_multi(expr);
                let written = values.iter().cloned().map(without_missing).collect();
                (values, written)
            }
        }
    }

    /// The type declared for the exports of `resource`, as `---@type PhoneExports` above
    /// `exports['phone'] = {}` declares it, when one is.
    pub fn declared_export_type(&self, resource: &str) -> Option<Type> {
        let declared = self.index.declared_exports_of(resource, self.ctx.file);
        declared.into_iter().map(|(_, symbol)| &symbol.ty).find(|ty| !holds_exports(ty)).cloned()
    }

    /// Every member of `ty` called `name`, as the definitions a call of it may reach.
    pub fn members_named(&self, ty: &Type, name: &str) -> Vec<MemberInfo> {
        self.guarded(|| self.members_matching(ty, Some(name)))
    }

    pub fn member(&self, ty: &Type, name: &str) -> Option<MemberInfo> {
        self.guarded(|| {
            let mut found = self.members_matching(ty, Some(name));
            if found.is_empty() {
                return indexed_field(name, self.index_value(ty, &Type::StringLit(SmolStr::new(name)), 0));
            }
            if let Some(merged) = merged_assignments(&found.iter().collect::<Vec<_>>()) {
                return Some(merged);
            }
            let best = (0..found.len()).max_by_key(|i| (found[*i].ty.specificity(), std::cmp::Reverse(*i)))?;
            Some(found.swap_remove(best))
        })
    }

    /// What a key of type `key` reads from a value of type `ty` through the indices that take it:
    /// the `[string]: integer` of `{ [string]: integer, name: string }`, the `---@field [string]
    /// integer` of a class or a parent, or the value type of `table<string, integer>`.
    fn index_value(&self, ty: &Type, key: &Type, depth: u32) -> Type {
        if depth > 8 {
            return Type::Unknown;
        }
        match ty {
            Type::Union(types) => Type::union(
                types.iter().filter(|t| !matches!(t, Type::Nil)).map(|t| self.index_value(t, key, depth + 1)),
            ),
            Type::Shape(shape) => {
                let values = shape.indices.iter().filter(|(index, _)| self.index_takes(index, key, 0));
                Type::union(values.map(|(_, value)| value.clone()))
            }
            Type::Map(index, value) if self.index_takes(index, key, 0) => (**value).clone(),
            Type::Named(name, args) if self.index.class(name, self.side).is_some() => {
                self.class_index_value(name, args, key, &mut FxHashSet::default(), depth + 1)
            }
            Type::Named(..) | Type::Require(_) => match self.resolve_alias(ty) {
                resolved if resolved != *ty => self.index_value(&resolved, key, depth + 1),
                _ => Type::Unknown,
            },
            _ => Type::Unknown,
        }
    }

    /// The members of `ty`, one for each name: the first found, or the values that several
    /// assignments store in it, as `member` gives them.
    pub fn members(&self, ty: &Type) -> Vec<MemberInfo> {
        let members = self.guarded(|| self.members_matching(ty, None));
        let mut names: Vec<&SmolStr> = Vec::new();
        let mut named: FxHashMap<&SmolStr, Vec<&MemberInfo>> = FxHashMap::default();
        for member in &members {
            named
                .entry(&member.name)
                .or_insert_with(|| {
                    names.push(&member.name);
                    Vec::new()
                })
                .push(member);
        }
        let first = |found: &[&MemberInfo]| merged_assignments(found).unwrap_or_else(|| found[0].clone());
        names.into_iter().map(|name| first(&named[name])).collect()
    }

    fn members_matching(&self, ty: &Type, filter: Option<&str>) -> Vec<MemberInfo> {
        let wanted = |name: &str| filter.is_none_or(|f| f == name);
        let mut out = Vec::new();
        match ty {
            Type::Union(types) => {
                for part in types.iter().filter(|t| !matches!(t, Type::Nil)) {
                    out.extend(self.guarded(|| self.members_matching(part, filter)));
                }
            }
            Type::Shape(shape) => {
                for field in shape.fields.iter().filter(|f| wanted(&f.name)) {
                    out.push(MemberInfo {
                        name: field.name.clone(),
                        ty: if field.optional { field.ty.clone().optional() } else { field.ty.clone() },
                        doc: None,
                        deprecated: false,
                        literal: None,
                        kind: SymbolKind::Field,
                        location: None,
                        inferred: false,
                    });
                }
            }
            Type::Named(name, args) => self.class_members(name, args, filter, &mut out, &mut FxHashSet::default(), 0),
            Type::GlobalTable(owner) => self.owner_members(owner, filter, &mut out),
            Type::Fun(fun) => {
                if let Some(owner) = &fun.fields {
                    self.owner_members(owner, filter, &mut out);
                }
            }
            Type::String | Type::StringLit(_) => {
                let library = self.global_type("string");
                if !matches!(library, Type::String | Type::StringLit(_)) {
                    out.extend(self.guarded(|| self.members_matching(&library, filter)));
                }
            }
            Type::Require(_) => {
                let resolved = self.resolve_alias(ty);
                if resolved != *ty {
                    out.extend(self.guarded(|| self.members_matching(&resolved, filter)));
                }
            }
            Type::Exports(None) => {
                for resource in self.index.resources.iter().filter(|r| wanted(&r.name)) {
                    out.push(MemberInfo {
                        name: resource.name.clone(),
                        ty: Type::Exports(Some(resource.name.clone())),
                        doc: Some(Arc::from(format!("Exports of the `{}` resource.", resource.name))),
                        deprecated: false,
                        literal: None,
                        kind: SymbolKind::Table,
                        location: None,
                        inferred: false,
                    });
                }
                // Resources the workspace lacks, such as escrowed ones, may still have typed exports.
                for (file, symbol) in self.index.declared_exports(self.ctx.file) {
                    if wanted(&symbol.name) && !out.iter().any(|m| m.name == symbol.name) {
                        let mut member = member_from_symbol(file, symbol);
                        member.ty = Type::Exports(Some(symbol.name.clone()));
                        member.kind = SymbolKind::Table;
                        member.doc = member
                            .doc
                            .or_else(|| Some(Arc::from(format!("Exports of the `{}` resource.", symbol.name))));
                        out.push(member);
                    }
                }
                if let Some(name) = filter.filter(|_| out.is_empty()) {
                    out.push(MemberInfo {
                        name: SmolStr::new(name),
                        ty: Type::Exports(Some(SmolStr::new(name))),
                        doc: None,
                        deprecated: false,
                        literal: None,
                        kind: SymbolKind::Table,
                        location: None,
                        inferred: false,
                    });
                }
            }
            Type::Exports(Some(resource)) => {
                // A declared type describes the exports on purpose, so it wins over the type
                // inferred from the function a resource registers under the same name.
                for (_, symbol) in self.index.declared_exports_of(resource, self.ctx.file) {
                    if !holds_exports(&symbol.ty) {
                        out.extend(self.guarded(|| self.members_matching(&symbol.ty, filter)));
                    }
                }
                let typed = out.len();
                for (file, symbol) in self.index.declared_export_members(resource, self.ctx.file) {
                    if !wanted(&symbol.name) || out[..typed].iter().any(|m| m.name == symbol.name) {
                        continue;
                    }
                    let declared = out[typed..].iter_mut().find(|m| m.name == symbol.name);
                    match (declared.map(|m| &mut m.ty), &symbol.ty) {
                        (Some(Type::Fun(fun)), Type::Fun(next)) => add_signature(fun, next),
                        (Some(_), _) => {}
                        (None, _) => out.push(member_from_symbol(file, symbol)),
                    }
                }
                let declared: FxHashSet<SmolStr> = out.iter().map(|m| m.name.clone()).collect();
                for (file, symbol) in self.index.exports_of(resource, filter) {
                    if !declared.contains(&symbol.name) {
                        let member = member_from_symbol(file, symbol);
                        out.push(MemberInfo { ty: called_through_exports(member.ty), ..member });
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// The members of the tables `owner` names, and of the tables they fall back on through the
    /// `__index` of their metatables for the fields they lack themselves.
    fn owner_members(&self, owner: &str, filter: Option<&str>, out: &mut Vec<MemberInfo>) {
        let start = out.len();
        self.own_members(owner, filter, out);
        let is_instance = instance_class(owner).is_some();
        if filter.is_some() && out.len() > start && !is_instance {
            return;
        }
        let fallback = self.fallback(owner);
        if fallback.is_unknown() || !self.following.borrow_mut().insert(SmolStr::new(owner)) {
            return;
        }
        let mut own: FxHashMap<SmolStr, usize> = FxHashMap::default();
        for (i, member) in out.iter().enumerate().skip(start) {
            own.entry(member.name.clone()).or_insert(i);
        }
        let inherited = self.guarded(|| self.members_matching(&fallback, filter));
        self.following.borrow_mut().remove(owner);
        for member in inherited {
            match own.get(&member.name) {
                // The methods of a class set the fields of its instances through `self`, so a field
                // that both set takes the more specific type, as `self.item = nil` in a constructor
                // does not hide the `Item` that `Holder:set(item)` stores.
                Some(&i) if is_instance && member.ty.specificity() > out[i].ty.specificity() => out[i] = member,
                Some(_) => {}
                None => out.push(member),
            }
        }
    }

    /// The tables that `owner` falls back on: its class for an instance, or the `__index` of the
    /// metatables that `setmetatable` gives it.
    fn fallback(&self, owner: &str) -> Type {
        // The instances of a class fall back on the class.
        if let Some(class) = instance_class(owner) {
            return Type::GlobalTable(SmolStr::new(class));
        }
        if !self.index.has_metatables(owner) {
            return Type::Unknown;
        }
        if let Some(ty) = self.fallbacks.borrow().get(owner) {
            return ty.clone();
        }
        let metatables = self.index.metatables_of(owner, self.ctx.file);
        if metatables.is_empty() || !self.following.borrow_mut().insert(SmolStr::new(owner)) {
            return Type::Unknown;
        }
        let (ty, complete) =
            self.complete(|| Type::union(metatables.into_iter().map(|metatable| self.metatable_index(metatable))));
        self.following.borrow_mut().remove(owner);
        if complete {
            self.fallbacks.borrow_mut().insert(SmolStr::new(owner), ty.clone());
        }
        ty
    }

    /// `ty`, a function the global path `path` holds, with the members set on that path, as a
    /// definition file sets `await` on `MySQL.query` with `function MySQL.query.await() end`.
    fn with_fields(&self, ty: Type, path: &str) -> Type {
        match ty {
            Type::Fun(fun) if fun.fields.is_none() && self.index.has_members(path) => {
                Type::Fun(Arc::new(FunType { fields: Some(SmolStr::new(path)), ..(*fun).clone() }))
            }
            other => other,
        }
    }

    /// The members set on the tables `owner` names themselves.
    fn own_members(&self, owner: &str, filter: Option<&str>, out: &mut Vec<MemberInfo>) {
        for (file, entry) in self.index.member_entries(owner, filter, self.ctx.file) {
            let symbol = &entry.symbol;
            if filter.is_none_or(|f| f == symbol.name) && applies_on(entry.side, self.side) {
                let mut member = member_from_symbol(file, symbol);
                member.inferred = !entry.typed && matches!(symbol.kind, SymbolKind::Field | SymbolKind::Variable);
                let nested = || format!("{owner}.{}", symbol.name);
                if matches!(member.ty, Type::Table | Type::Unknown) {
                    let nested = nested();
                    if self.index.has_members(&nested) {
                        member.ty = Type::GlobalTable(SmolStr::new(nested));
                    }
                } else if matches!(member.ty, Type::Fun(_)) {
                    member.ty = self.with_fields(member.ty, &nested());
                }
                out.push(member);
            }
        }
        if let Some(name) = filter {
            let nested = format!("{owner}.{name}");
            if out.is_empty() && self.index.has_members(&nested) {
                out.push(MemberInfo {
                    name: SmolStr::new(name),
                    ty: Type::GlobalTable(SmolStr::new(nested)),
                    doc: None,
                    deprecated: false,
                    literal: None,
                    kind: SymbolKind::Table,
                    location: None,
                    inferred: false,
                });
            }
        }
    }

    /// The file whose view of the class name `name`, read in `from`, decides which declarations it
    /// stands for: the file that wrote it, when the type comes from the index entry of a file of
    /// another resource that `from` does not import, or else `from`. Definition files and stubs
    /// outside resources describe their classes for every resource, and an imported file runs as
    /// part of the resource that imports it, so the names they write decide nothing.
    pub fn view_of(&self, name: &TypeName, from: FileId) -> FileId {
        let resource = |file: FileId| self.index.file(file).and_then(|entry| entry.resource);
        match name.origin {
            Some(origin)
                if resource(origin).is_some()
                    && resource(origin) != resource(from)
                    && !self.index.is_visible(from, origin) =>
            {
                origin
            }
            _ => from,
        }
    }

    /// The members of the class or alias `name` given the type arguments `args`, with those of its
    /// parents: the fields of a class, those set on the table its `---@class` declares, and the
    /// fields of a table type it names as a parent, like `{ name: string }`. A class that several
    /// parents share is read once, and so is one that names itself as a parent with other type
    /// arguments, as `---@class Tree<T> : Tree<T[]>` does.
    fn class_members(
        &self,
        name: &TypeName,
        args: &[Type],
        filter: Option<&str>,
        out: &mut Vec<MemberInfo>,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) {
        if depth > 8 || !visited.insert(name.text.clone()) {
            return;
        }
        let mut defs = self.index.class_defs(name);
        defs.retain(|(_, class)| applies_on(class.side, self.side));
        if defs.is_empty() {
            if let Some((_, alias)) = self.index.alias(name, self.side) {
                let ty = substitute(&alias.ty, &expanded_bindings(&alias.generics, args));
                out.extend(self.guarded(|| self.members_matching(&ty, filter)));
            }
            return;
        }
        // Resources may declare classes of the same name differently. When the resource of the file
        // whose view decides, as `view_of` finds it, describes the class in its `---@meta` files, its
        // scripts for the side of that file or the modules it loads through `files` or `require`,
        // those declarations give the members, and otherwise all of them do: the `Player` that
        // `exports.qbx_core:GetPlayer()` gives is qbx_core's. A declaration that only names the
        // class, as `---@class Player` above code that uses those players does, describes nothing.
        let view = self.view_of(name, self.ctx.file);
        let entry = |file: FileId| self.index.file(file);
        let (resource, side) = entry(view).map_or((None, None), |entry| (entry.resource, entry.side));
        let own = |file: FileId| {
            file == view
                || entry(file).is_some_and(|other| {
                    resource.is_some()
                        && other.resource == resource
                        && (other.index.meta
                            || other.side.is_none_or(|theirs| side.is_none_or(|ours| theirs.is_available_on(ours))))
                })
        };
        let describes = |class: &ClassDef| {
            !class.fields.is_empty()
                || !class.indices.is_empty()
                || !class.literal_fields.is_empty()
                || !class.parent_types.is_empty()
                || class.call.is_some()
        };
        if defs.iter().any(|(file, class)| own(*file) && describes(class)) {
            defs.retain(|(file, _)| own(*file));
        }
        let own = out.len();
        for (file, class) in &defs {
            let sides = class.field_sides.iter().copied().chain(std::iter::repeat(None));
            for (field, side) in class.fields.iter().zip(sides) {
                if filter.is_none_or(|n| n == field.name) && applies_on(side, self.side) {
                    out.push(member_from_symbol(*file, field));
                }
            }
        }
        self.owner_members(name, filter, out);
        let bindings = class_bindings(&defs, args);
        if !bindings.is_empty() {
            for member in &mut out[own..] {
                member.ty = substitute(&member.ty, &bindings);
            }
        }
        for (_, parent) in bound_parents(&defs, args) {
            match parent {
                Type::Named(parent, args) => self.class_members(&parent, &args, filter, out, visited, depth + 1),
                shape @ Type::Shape(_) => out.extend(self.guarded(|| self.members_matching(&shape, filter))),
                _ => {}
            }
        }
    }
}

/// Whether a declaration of a global as `ty` tells what the global holds. `Zone = nil` declares a
/// global that some handler sets later, not what it holds.
fn tells_global_value(ty: &Type) -> bool {
    !ty.is_unknown() && !matches!(ty, Type::Nil)
}

/// How much a global declared as `ty` says about its table: 2 for a type that is or may be a class,
/// 1 for a global table whose members its files set, and 0 for anything else.
fn table_rank(ty: &Type) -> u8 {
    match ty {
        Type::Named(..) => 2,
        Type::GlobalTable(_) => 1,
        Type::Union(parts) => parts.iter().map(table_rank).max().unwrap_or(0),
        _ => 0,
    }
}

/// Whether `ty` is the exports of a resource, as `exports.phone = saved` holds after
/// `local saved = exports.phone`. Such a value adds nothing to the exports it is assigned to.
fn holds_exports(ty: &Type) -> bool {
    match ty {
        Type::Exports(_) => true,
        Type::Union(types) => types.iter().any(holds_exports),
        _ => false,
    }
}

/// Adds `next` and its overloads to the signatures of `fun`, as a method that a definition file
/// declares twice, like `Search` of `ox_inventory` for `'count'` and for `'slots'`, has both.
fn add_signature(fun: &mut Arc<FunType>, next: &Arc<FunType>) {
    let fun = Arc::make_mut(fun);
    fun.overloads.push(Arc::new(FunType { overloads: Vec::new(), ..(**next).clone() }));
    fun.overloads.extend(next.overloads.iter().cloned());
}

/// A field that several assignments without a `---@type` set, as `T.state = 0` and
/// `self.state = 'running'` do, which holds any of the values they store besides the `nil` that
/// clears it, as lua-language-server reads it. `None` unless every one of `found` is such an
/// assignment, and two or more of them store a value, none of them a function.
fn merged_assignments(found: &[&MemberInfo]) -> Option<MemberInfo> {
    if found.len() < 2 || !found.iter().all(|member| member.inferred) {
        return None;
    }
    let stored: Vec<&MemberInfo> = found.iter().copied().filter(|member| member.ty != Type::Nil).collect();
    if stored.len() < 2 || stored.iter().any(|member| member.ty.as_fun().is_some()) {
        return None;
    }
    // `1` and `1.5` store numbers, not `integer|number`. A value that may be anything, like the
    // `args[1]` of an untyped command handler, tells no more about the field than one of unknown
    // type does.
    let has_number = stored.iter().any(|member| matches!(member.ty.without_nil(), Type::Number));
    let all_any = stored.iter().all(|member| member.ty == Type::Any);
    let types = stored
        .iter()
        .map(|member| member.ty.clone())
        .filter(|ty| !(has_number && *ty == Type::Integer) && (all_any || *ty != Type::Any));
    let mut literals: Vec<SmolStr> = Vec::new();
    for literal in stored.iter().map(|member| member.literal.clone()) {
        match literal {
            Some(literal) if !literals.contains(&literal) => literals.push(literal),
            Some(_) => {}
            None => {
                literals.clear();
                break;
            }
        }
    }
    let more = if literals.len() > MAX_MERGED_LITERALS { "|..." } else { "" };
    literals.truncate(MAX_MERGED_LITERALS);
    let literal = (!literals.is_empty()).then(|| SmolStr::new(format!("{}{more}", literals.join("|"))));
    Some(MemberInfo { ty: Type::union(types), literal, ..stored[0].clone() })
}

/// The field `name` that an index gives the value `ty`, unless the value is unknown.
fn indexed_field(name: &str, ty: Type) -> Option<MemberInfo> {
    (!ty.is_unknown()).then(|| MemberInfo {
        name: SmolStr::new(name),
        ty,
        doc: None,
        deprecated: false,
        literal: None,
        kind: SymbolKind::Field,
        location: None,
        inferred: false,
    })
}

/// A function registered with `exports('Name', fn)` as calls through `exports.<resource>` see it.
/// The proxy drops the first value of the call, so `exports.res:Fn(a, b)` and `exports.res.Fn(x, a, b)`
/// both pass `a, b`, lined up with the parameters the way a function declared with `:` takes them.
fn called_through_exports(ty: Type) -> Type {
    match ty {
        Type::Fun(fun) => Type::Fun(proxied(&fun)),
        Type::Union(types) => Type::Union(types.into_iter().map(called_through_exports).collect()),
        other => other,
    }
}

fn proxied(fun: &FunType) -> Arc<FunType> {
    let overloads = fun.overloads.iter().map(|overload| proxied(overload)).collect();
    Arc::new(FunType { is_method: true, lists_receiver: false, overloads, ..fun.clone() })
}

fn member_from_symbol(file: FileId, symbol: &crate::index::Symbol) -> MemberInfo {
    MemberInfo {
        name: symbol.name.clone(),
        ty: symbol.ty.clone(),
        doc: symbol.doc.clone(),
        deprecated: symbol.deprecated,
        literal: symbol.literal.clone(),
        kind: symbol.kind,
        location: Some((file, symbol.range)),
        inferred: false,
    }
}

/// What the type parameters of a class stand for in a reference that gives them `args`: `string`
/// for the `T` of `List<string>`. A parameter left without an argument stays itself, as the `T`
/// of a `---@type List` does, which lua-language-server shows as `<T>`: a field of that type takes
/// any value but is still required, and a method's own `@generic T` binds from the call. The table
/// a `---@class` declares, and `self` in its methods, keep the parameters too, being `List<T>`.
/// Binding `T` to itself would change nothing.
pub(crate) fn generic_bindings(params: &[SmolStr], args: &[Type]) -> Vec<(SmolStr, Type)> {
    let itself =
        |param: &SmolStr, arg: &Type| matches!(arg, Type::Named(name, args) if name == param && args.is_empty());
    params.iter().cloned().zip(args.iter().cloned()).filter(|(param, arg)| !itself(param, arg)).collect()
}

/// What the type parameters of an alias stand for where its type is read in place, as `Box<integer>`
/// reads the `{ value: T }` of `---@alias Box<T> { value: T }`: those `generic_bindings` binds,
/// with a parameter left without an argument unknown, as lua-language-server reads the `value` of
/// a `---@type Box`.
pub(crate) fn expanded_bindings(params: &[SmolStr], args: &[Type]) -> Vec<(SmolStr, Type)> {
    let missing = params.iter().skip(args.len()).map(|param| (param.clone(), Type::Unknown));
    generic_bindings(params, args).into_iter().chain(missing).collect()
}

/// The type parameters of a class bound to `args`, as the declaration of it among `defs` that
/// lists them declares them.
pub(crate) fn class_bindings(defs: &[(FileId, &ClassDef)], args: &[Type]) -> Vec<(SmolStr, Type)> {
    generic_bindings(class_params(defs), args)
}

/// The type parameters of a class, as the declaration of it among `defs` that lists them declares
/// them.
fn class_params<'d>(defs: &[(FileId, &'d ClassDef)]) -> &'d [SmolStr] {
    defs.iter().find(|(_, def)| !def.generics.is_empty()).map_or(&[], |(_, def)| &def.generics)
}

/// The parents that the declarations of a class among `defs` name, with the file of each, given
/// the type arguments `args` of the class: `Parent<string>` for the `Parent<T>` of a
/// `Child<string>`. A table type named as a parent, like the `{ [number]: T }` of
/// `---@class Array<T> : { [number]: T }`, is read in place as an alias is, so a parameter without
/// an argument is unknown in it, as lua-language-server reads `array[1]` of a `---@type Array`.
pub(crate) fn bound_parents(defs: &[(FileId, &ClassDef)], args: &[Type]) -> Vec<(FileId, Type)> {
    let params = class_params(defs);
    let (named, table) = (generic_bindings(params, args), expanded_bindings(params, args));
    let parents = defs.iter().flat_map(|(file, def)| def.parent_types.iter().map(move |parent| (*file, parent)));
    parents
        .map(|(file, parent)| match parent {
            Type::Named(..) => (file, substitute(parent, &named)),
            other => (file, substitute(other, &table)),
        })
        .collect()
}

pub(crate) fn substitute(ty: &Type, generics: &[(SmolStr, Type)]) -> Type {
    if generics.is_empty() {
        return ty.clone();
    }
    let bound =
        |name: &str| generics.iter().find(|(n, _)| n == name).map_or_else(|| ty.clone(), |(_, bound)| bound.clone());
    match ty {
        Type::Named(name, args) if args.is_empty() => bound(name),
        // A returned `` `T` `` is the class that the string passed for it names.
        Type::NameOf(name) => bound(name),
        Type::Named(name, args) => Type::Named(name.clone(), args.iter().map(|t| substitute(t, generics)).collect()),
        Type::Array(inner) => Type::Array(Box::new(substitute(inner, generics))),
        Type::Tuple(items) => Type::Tuple(items.iter().map(|t| substitute(t, generics)).collect()),
        Type::Variadic(inner) => Type::Variadic(Box::new(substitute(inner, generics))),
        // `V?` with `V` unbound is unknown, not `nil`, while `V|string` is still a `string`.
        Type::Union(types) => {
            let parts: Vec<Type> = types.iter().map(|t| substitute(t, generics)).collect();
            if parts.iter().filter(|t| !matches!(t, Type::Nil)).all(Type::is_unknown) {
                Type::Unknown
            } else {
                ty.rebuilt(parts)
            }
        }
        Type::Map(k, v) => Type::Map(Box::new(substitute(k, generics)), Box::new(substitute(v, generics))),
        Type::Fun(fun) => Type::Fun(Arc::new(substitute_fun(fun, generics))),
        Type::Shape(shape) => Type::Shape(Arc::new(Shape {
            fields: shape.fields.iter().map(|f| ShapeField { ty: substitute(&f.ty, generics), ..f.clone() }).collect(),
            array: shape.array.as_ref().map(|t| substitute(t, generics)),
            indices: shape.indices.iter().map(|(k, v)| (substitute(k, generics), substitute(v, generics))).collect(),
        })),
        other => other.clone(),
    }
}

/// `fun` with `generics` bound in its parameters, returned values and overloads, as a method of
/// `List<T>` read from a `List<string>`. The names it binds are no longer generics of `fun`, even
/// when `fun` declares one of them with `@generic` too, as lua-language-server binds them.
fn substitute_fun(fun: &FunType, generics: &[(SmolStr, Type)]) -> FunType {
    let types = |types: &[Type]| -> Vec<Type> { types.iter().map(|t| substitute(t, generics)).collect() };
    let unbound = |name: &&SmolStr| !generics.iter().any(|(bound, _)| bound == *name);
    let returns = types(&fun.returns);
    let return_sets: Vec<Vec<Type>> = fun.return_sets.iter().map(|set| types(set)).collect();
    // Values bound from the arguments of a call tell what it passes today, not what it may,
    // as the values that a generic function returns do.
    let bound = returns != fun.returns || return_sets != fun.return_sets;
    FunType {
        params: fun.params.iter().map(|p| Param { ty: substitute(&p.ty, generics), ..p.clone() }).collect(),
        returns,
        return_values: fun.return_values.clone(),
        return_sets,
        returns_inferred: fun.returns_inferred || bound,
        explicit_returns: fun.explicit_returns.as_ref().map(|returns| types(returns)),
        is_method: fun.is_method,
        lists_receiver: fun.lists_receiver,
        generics: fun.generics.iter().filter(unbound).cloned().collect(),
        overloads: fun.overloads.iter().map(|overload| Arc::new(substitute_fun(overload, generics))).collect(),
        side: fun.side,
        callback: fun.callback.clone(),
        nodiscard: fun.nodiscard,
        is_async: fun.is_async,
        returns_nothing: fun.returns_nothing,
        fields: fun.fields.clone(),
    }
}

/// The constructor behind a table-valued initialiser, looking through `setmetatable({...}, mt)`.
pub fn table_fields(expr: &Expr) -> Option<&[TableField]> {
    match &expr.unparen().kind {
        ExprKind::Table(fields) => Some(fields),
        ExprKind::Call { callee, args, .. } if callee.dotted_path().as_deref() == Some("setmetatable") => {
            args.first().and_then(table_fields)
        }
        _ => None,
    }
}

/// The instances of the tables in `index`, where each table a class is indexed as becomes the owner
/// of its instances' own fields.
fn instances_of(index: Type) -> Type {
    match index {
        Type::GlobalTable(owner) => Type::GlobalTable(instance_owner(&owner)),
        Type::Union(types) => Type::union(types.into_iter().map(instances_of)),
        other => other,
    }
}

/// The metatables that the `setmetatable({...}, mt)` calls of a table-valued initialiser give the
/// table it builds.
pub fn metatable_args(expr: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    let mut current = expr.unparen();
    while let ExprKind::Call { callee, args, .. } = &current.kind {
        let (Some("setmetatable"), Some(table)) = (callee.dotted_path().as_deref(), args.first()) else { break };
        out.extend(args.get(1));
        current = table.unparen();
    }
    out
}

/// The entries of a table constructor that have no name: the array part `ipairs` visits, which
/// takes in `[n]` keys that continue it as in `{ [1] = 'a', [2] = 'b' }`, and the other `[key]`s.
pub struct TableElements<'a> {
    pub array: Vec<&'a Expr>,
    pub keyed: Vec<(&'a Expr, &'a Expr)>,
}

pub fn table_elements(fields: &[TableField]) -> TableElements<'_> {
    let mut elements = TableElements { array: Vec::new(), keyed: Vec::new() };
    let mut numbered = Vec::new();
    for field in fields {
        match field {
            TableField::Positional(value) => elements.array.push(value),
            TableField::Keyed { key, value } => match &key.kind {
                ExprKind::String(_) => {}
                ExprKind::Number(NumberValue::Int(n)) => numbered.push((*n, key, value)),
                _ => elements.keyed.push((key, value)),
            },
            TableField::Named { .. } | TableField::SetMember(_) => {}
        }
    }
    let mut length = elements.array.len() as i64;
    while numbered.iter().any(|(n, ..)| *n == length + 1) {
        length += 1;
    }
    for (n, key, value) in numbered {
        if (1..=length).contains(&n) {
            elements.array.push(value);
        } else {
            elements.keyed.push((key, value));
        }
    }
    elements
}

/// Whether a key of type `key` is a number, or may be one as far as is known: `integer`, `number`, a
/// union of them, `any` or `unknown`.
fn is_number_key(key: &Type) -> bool {
    let number =
        |part: &Type| matches!(part, Type::Integer | Type::Number | Type::IntLit(_) | Type::Any | Type::Unknown);
    match key {
        Type::Union(parts) => parts.iter().all(number),
        one => number(one),
    }
}

fn is_integer_key(key: &Type) -> bool {
    matches!(key.widen(), Type::Integer | Type::Number)
}

/// Whether a key of type `key` may be the `literal` that a `---@field [1] number` is keyed by:
/// `1` itself, a key of its kind such as `integer`, or a key of unknown type.
fn may_be_literal_key(key: &Type, literal: &Type) -> bool {
    match key {
        Type::Union(types) => types.iter().any(|part| may_be_literal_key(part, literal)),
        Type::Unknown | Type::Any => true,
        Type::Number => matches!(literal, Type::IntLit(_)),
        key if key.is_literal() => key == literal,
        key => *key == literal.widen(),
    }
}

/// Whether the `-T` entry of a `---@cast` line for `removed` takes `part` out of a union: the same
/// type, or a literal or integer of the kind `removed` names.
fn covers(removed: &Type, part: &Type) -> bool {
    removed == part
        || (!removed.is_literal() && part.widen() == *removed)
        || (*removed == Type::Number && matches!(part, Type::Integer | Type::IntLit(_) | Type::Handle(_)))
}

/// What one position holds across several lists of returned values.
/// `ty` without the `nil` or `false` it may hold, as a value that a `return` passes without writing
/// either: `unknown` when it holds nothing else.
fn without_missing(ty: Type) -> Type {
    let missing = |part: &Type| matches!(part, Type::Nil | Type::BooleanLit(false));
    match &ty {
        Type::Union(parts) => ty.rebuilt(parts.iter().filter(|part| !missing(part)).cloned()),
        one if missing(one) => Type::Unknown,
        _ => ty,
    }
}

fn merge_values(parts: Vec<Type>) -> Type {
    // Merged with `nil`, a value that cannot be inferred would read as `nil`.
    if parts.iter().any(Type::is_unknown) {
        return Type::Unknown;
    }
    // `return 1` and `return 0.5` return numbers, not `integer|number`.
    let has_number = parts.iter().any(|t| matches!(t.without_nil(), Type::Number));
    Type::union(parts.into_iter().filter(|t| !(has_number && matches!(t, Type::Integer))))
}

/// The `return` statements of a function body with the values each passes, leaving out those of
/// the functions defined in it.
pub fn return_stmts(body: &Block) -> Vec<(&Stmt, &[Expr])> {
    let mut out = Vec::new();
    collect_returns(body, &mut out);
    out
}

/// The expressions of the first `return` that belongs to this function body itself.
/// The values of every `return` in a function body, leaving out nested functions.
fn collect_returns<'b>(block: &'b Block, out: &mut Vec<(&'b Stmt, &'b [Expr])>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            StmtKind::Return(exprs) => out.push((stmt, exprs)),
            StmtKind::Do(body) | StmtKind::While { body, .. } | StmtKind::Repeat { body, .. } => {
                collect_returns(body, out)
            }
            StmtKind::NumericFor { body, .. } | StmtKind::GenericFor { body, .. } => collect_returns(body, out),
            StmtKind::If { branches, else_block } => {
                for branch in branches {
                    collect_returns(&branch.block, out);
                }
                if let Some(block) = else_block {
                    collect_returns(block, out);
                }
            }
            _ => {}
        }
    }
}

/// Whether running `block` always ends in a `return`, an `error(...)` call, a loop that only a
/// `return` leaves, such as `while true do` without `break`, or a `goto`, so a function never runs
/// past the end of it.
pub fn always_exits(block: &Block) -> bool {
    match block.stmts.last().map(|stmt| &stmt.kind) {
        Some(StmtKind::Return(_) | StmtKind::Goto(_)) => true,
        Some(StmtKind::Expr(Expr { kind: ExprKind::Call { callee, .. }, .. })) => {
            callee.dotted_path().as_deref() == Some("error")
        }
        Some(StmtKind::Do(body)) => always_exits(body),
        Some(StmtKind::If { branches, else_block: Some(else_block) }) => {
            branches.iter().all(|branch| always_exits(&branch.block)) && always_exits(else_block)
        }
        Some(StmtKind::While { cond, body }) => matches!(cond.unparen().kind, ExprKind::True) && !breaks(body),
        Some(StmtKind::Repeat { body, cond }) => {
            (always_exits(body) || matches!(cond.unparen().kind, ExprKind::False | ExprKind::Nil)) && !breaks(body)
        }
        _ => false,
    }
}

/// Whether a loop body has a `break` that leaves that loop rather than one inside it.
fn breaks(block: &Block) -> bool {
    block.stmts.iter().any(|stmt| match &stmt.kind {
        StmtKind::Break => true,
        StmtKind::Do(body) => breaks(body),
        StmtKind::If { branches, else_block } => {
            branches.iter().any(|branch| breaks(&branch.block)) || else_block.as_ref().is_some_and(breaks)
        }
        _ => false,
    })
}

/// Where a table field starts, whose doc comment the function it holds takes: at its name, the `[`
/// of its key, or its value.
fn field_start(source: &str, field: &TableField) -> u32 {
    match field {
        TableField::Named { name, .. } | TableField::SetMember(name) => name.span.start,
        TableField::Keyed { key, .. } => {
            let before = source[..key.span.start as usize].trim_end();
            before.strip_suffix('[').map_or(key.span.start, |rest| rest.len() as u32)
        }
        TableField::Positional(value) => value.span.start,
    }
}

/// `declared` without what `facts` rule out, as `Infer::facts_left` reads them: when they rule out
/// every value of it, `Err` with the type that the code they guard takes the value to be then, as
/// `Fact::ruled_out` reads it from the guard that rules out the last of them, if it gives one.
fn narrowed_by<'f>(declared: &Type, mut facts: impl Iterator<Item = Cow<'f, Fact>>) -> Result<Type, Option<Type>> {
    let mut narrowed = declared.clone();
    while let Some(fact) = facts.next() {
        match fact.assume(&narrowed) {
            Some(left) => narrowed = left,
            // The guards after it tell about what the code takes the value to be.
            None => {
                let taken = fact.ruled_out(&narrowed);
                return Err(taken.and_then(|taken| facts.try_fold(taken, |ty, fact| fact.assume(&ty))));
            }
        }
    }
    Ok(narrowed)
}

/// The function literal that `expr` gives when it is the value, as `function() end` itself,
/// `X or function() end` and `cond and function() end` may.
fn alternative_function(expr: &Expr) -> Option<&FuncBody> {
    match &expr.kind {
        ExprKind::Function(func) => Some(func),
        ExprKind::Paren(inner) => alternative_function(inner),
        ExprKind::Binary { op: BinOp::Or, lhs, rhs, .. } => {
            alternative_function(rhs).or_else(|| alternative_function(lhs))
        }
        ExprKind::Binary { op: BinOp::And, rhs, .. } => alternative_function(rhs),
        _ => None,
    }
}

/// The value at `index` of those that `exprs` assign, as `single` and `multi` type the expressions:
/// the expression at that place, with a literal widened as `mode = 'dev'` stores a setting rather
/// than the only value it may hold, a value of the call that ends them, or `nil`.
pub fn assigned_value(
    exprs: &[Expr],
    index: usize,
    single: impl Fn(&Expr) -> Type,
    multi: impl Fn(&Expr) -> Vec<Type>,
) -> Type {
    match exprs.get(index) {
        Some(expr) if index + 1 == exprs.len() && expr.is_multi_value() => value_at(multi(expr), 0).unwrap_or_default(),
        Some(expr)
            if matches!(
                expr.unparen().kind,
                ExprKind::True | ExprKind::False | ExprKind::Number(_) | ExprKind::String(_)
            ) =>
        {
            single(expr).widen()
        }
        Some(expr) => single(expr),
        None => match exprs.last() {
            Some(last) if last.is_multi_value() => value_at(multi(last), index + 1 - exprs.len()).unwrap_or_default(),
            _ => Type::Nil,
        },
    }
}

/// The value at `position` of `values`, the values an expression gives. A `...T` that ends them
/// gives a `T` there and at each position after it, as `local job, grade = table.unpack(t)` does.
pub fn value_at(mut values: Vec<Type>, position: usize) -> Option<Type> {
    let open = matches!(values.last(), Some(Type::Variadic(_)));
    let position = if open { position.min(values.len() - 1) } else { position };
    if position >= values.len() {
        return None;
    }
    match values.swap_remove(position) {
        Type::Variadic(inner) => Some(*inner),
        ty => Some(ty),
    }
}

/// The types of the values that each set of `Origin::Others` of a local stands for, each once, by
/// the local and the origin of the set.
pub type SetTypes = FxHashMap<(LocalId, u32), Rc<[Type]>>;

/// The types of the values that one of the values a local may hold stands for: that of where it
/// comes from, or for a set of `Origin::Others` those of the values in it, each once.
pub enum Bases {
    One(Type),
    Set(Rc<[Type]>),
}

impl Bases {
    pub fn iter(&self) -> std::slice::Iter<'_, Type> {
        match self {
            Bases::One(ty) => std::slice::from_ref(ty).iter(),
            Bases::Set(types) => types.iter(),
        }
    }
}

/// The types that `given` finds, each once, and whether it found each, rather than leaving out one
/// that is being found.
pub fn distinct_bases(given: impl Iterator<Item = Option<Type>>) -> (Rc<[Type]>, bool) {
    let (mut types, mut complete) = (Vec::new(), true);
    for ty in given {
        match ty {
            Some(ty) if !types.contains(&ty) => types.push(ty),
            Some(_) => {}
            None => complete = false,
        }
    }
    (types.into(), complete)
}

/// The table that the generic `for` statement `stmt` goes through with `pairs`, `ipairs`, `next` or
/// `each`, and whether it does with `ipairs`.
pub fn iterated_table(stmt: &Stmt) -> Option<(&Expr, bool)> {
    let StmtKind::GenericFor { exprs, .. } = &stmt.kind else { return None };
    let first = exprs.first()?;
    match &first.kind {
        ExprKind::Call { callee, args, .. } => match (callee.dotted_path().as_deref(), args.first()) {
            (Some(iterator @ ("pairs" | "ipairs" | "next" | "each")), Some(arg)) => Some((arg, iterator == "ipairs")),
            _ => None,
        },
        // `for k, v in next, t`
        _ if first.dotted_path().as_deref() == Some("next") => exprs.get(1).map(|arg| (arg, false)),
        _ => None,
    }
}

/// The union of the types of the values a local may hold. A literal gives way to its kind when
/// another value is of that kind, so `"active"|"busy"` and a `string` assigned later make a `string`,
/// an `integer` gives way to a `number`, and a `true` and a `false` make a `boolean`.
fn merge_versions(types: Vec<Type>) -> Type {
    if types.len() < 2 {
        return types.into_iter().next().unwrap_or_default();
    }
    let parts = |ty: &Type| match ty {
        Type::Union(parts) => parts.clone(),
        one => vec![one.clone()],
    };
    let absorbs = |kind: &Type, part: &Type| {
        matches!(
            (kind, part),
            (Type::String, Type::StringLit(_))
                | (Type::Integer | Type::Number, Type::IntLit(_))
                | (Type::Number, Type::Integer)
                | (Type::Boolean, Type::BooleanLit(_))
        )
    };
    let all: Vec<Vec<Type>> = types.iter().map(parts).collect();
    let merged = types.iter().enumerate().map(|(index, ty)| {
        let absorbed = |part: &&Type| {
            let others = all.iter().enumerate().filter(|(other, _)| *other != index);
            others.flat_map(|(_, kinds)| kinds).any(|kind| absorbs(kind, part))
        };
        ty.rebuilt(all[index].iter().filter(|part| !absorbed(part)).cloned())
    });
    booleans_joined(Type::union(merged))
}

/// `ty` with the strings and numbers it lists widened to their kinds, as a value written out is
/// shown, while `true` and `false` stay: they tell which way an `and` or `or` goes.
fn widened_values(ty: &Type) -> Type {
    match ty {
        Type::Union(parts) => ty.rebuilt(parts.iter().map(widened_values)),
        Type::BooleanLit(_) => ty.clone(),
        other => other.widen(),
    }
}

/// `ty` with a `true` and a `false` it lists, or either beside a `boolean`, joined into a `boolean`.
fn booleans_joined(ty: Type) -> Type {
    let Type::Union(parts) = &ty else { return ty };
    let both = parts.contains(&Type::BooleanLit(true)) && parts.contains(&Type::BooleanLit(false));
    let literal = parts.iter().any(|part| matches!(part, Type::BooleanLit(_)));
    if !(both || (literal && parts.contains(&Type::Boolean))) {
        return ty;
    }
    let joined = parts.iter().map(|part| match part {
        Type::BooleanLit(_) => Type::Boolean,
        other => other.clone(),
    });
    ty.rebuilt(joined)
}

/// The `---@type` that `doc` gives the name at `index` of the `local` statement below it. A local
/// is no member of a class, so a `self` in the type stands for nothing and is unknown, as in
/// lua-language-server.
fn local_annotation(doc: &DocGroup, index: usize) -> Option<Type> {
    let ty = doc.type_at(index)?;
    Some(if ty.mentions_self() { ty.with_self(&Type::Unknown) } else { ty.clone() })
}

/// The functions a statement defines under its doc comment: `function f()`, `local function f()`,
/// function literals assigned directly, `local f = function()` or `M.f = function()`, and those
/// passed to the call the statement makes or assigns, `RegisterServerCallback('name', function()`
/// or `handlers[name] = RegisterNetEvent(name, function()`. These are the functions whose
/// parameters take the `@param` lines above the statement.
pub fn documented_functions(stmt: &Stmt) -> Vec<&FuncBody> {
    match &stmt.kind {
        StmtKind::Function { func, .. } | StmtKind::LocalFunction { func, .. } => vec![func],
        StmtKind::Local { exprs, .. } | StmtKind::Assign { exprs, .. } => exprs
            .iter()
            .flat_map(|expr| match &expr.kind {
                ExprKind::Function(func) => vec![&**func],
                _ => passed_functions(expr),
            })
            .collect(),
        StmtKind::Expr(expr) => passed_functions(expr),
        _ => Vec::new(),
    }
}

/// The function literals among the arguments of `expr`, when it is a call.
fn passed_functions(expr: &Expr) -> Vec<&FuncBody> {
    let (ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. }) = &expr.kind else { return Vec::new() };
    args.iter()
        .filter_map(|arg| match &arg.kind {
            ExprKind::Function(func) => Some(&**func),
            _ => None,
        })
        .collect()
}
