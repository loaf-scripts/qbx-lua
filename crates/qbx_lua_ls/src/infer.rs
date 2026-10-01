use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use lsp_types::Range;
use qbx_fivem_data::{native, Side};
use qbx_lua_analysis::env::leading_doc_lines;
use qbx_lua_analysis::scope::{LocalId, LocalKind, Resolution, Resolved};
use qbx_lua_analysis::side_guard::SideRegions;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{CommentKind, NumberValue, SmolStr};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::callback_wrappers::{self, Wrapper};
use crate::index::{FileId, Index, SymbolKind};
use crate::locate::statement_at;
use crate::luacats::{applies_on, parse_doc_lines, DocGroup};
use crate::narrow::Guards;
use crate::types::{CallbackRole, FunType, Param, Shape, ShapeField, Type, TypeParser};

const MAX_DEPTH: u32 = 24;
const MAX_SHAPE_FIELDS: usize = 96;
/// An undocumented function that returns more different sets of values than this keeps only what
/// each position holds across them.
const MAX_RETURN_SETS: usize = 4;

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
    /// Value `index` of a `local` statement or an assignment, typed by the `---@type` above it.
    Value { stmt: &'a Stmt, index: usize },
    /// The value of the field `key` of a table constructor, which has the type of that field of the
    /// table.
    Field { table: &'a Expr, key: FieldKey<'a> },
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
    docs: RefCell<FxHashMap<u32, Rc<DocGroup>>>,
    guards: OnceCell<Guards>,
}

impl<'a> FileContext<'a> {
    pub fn new(file: FileId, source: &'a str, chunk: &'a Chunk, resolution: &'a Resolution) -> Self {
        let mut collector = DeclCollector {
            source,
            decls: FxHashMap::default(),
            call_anchors: FxHashMap::default(),
            tables: FxHashMap::default(),
        };
        collector.block(&chunk.block);
        Self {
            file,
            source,
            chunk,
            resolution,
            decls: collector.decls,
            call_anchors: collector.call_anchors,
            tables: collector.tables,
            docs: RefCell::new(FxHashMap::default()),
            guards: OnceCell::new(),
        }
    }

    /// What the conditions of the file tell about the locals they test.
    pub fn guards(&self) -> &Guards {
        self.guards.get_or_init(|| Guards::of(self.chunk, self.resolution))
    }

    pub fn decl(&self, decl_start: u32) -> Option<&Decl<'a>> {
        self.decls.get(&decl_start)
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
    fn local_owner_decl(&self, owner: &str) -> Option<u32> {
        let (file, decl_start) = owner.strip_prefix("%f")?.split_once(':')?;
        (file.parse::<FileId>().ok()? == self.file).then(|| decl_start.parse().ok()).flatten()
    }
}

struct DeclCollector<'a> {
    source: &'a str,
    decls: FxHashMap<u32, Decl<'a>>,
    call_anchors: FxHashMap<u32, u32>,
    tables: FxHashMap<u32, Expected<'a>>,
}

impl<'a> DeclCollector<'a> {
    fn block(&mut self, block: &'a Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    fn func(&mut self, func: &'a FuncBody, doc_anchor: Option<u32>, expected: Option<Expected<'a>>) {
        for (index, param) in func.params.iter().enumerate() {
            self.decls.insert(param.span.start, Decl::Param { func, index, doc_anchor, expected });
        }
        self.block(&func.body);
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
                self.func(func, anchor, None);
            }
            StmtKind::Assign { targets, exprs } => {
                targets.iter().for_each(|e| self.expr(e, None));
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
            StmtKind::Return(exprs) => exprs.iter().for_each(|e| self.expr(e, None)),
            StmtKind::Break | StmtKind::Goto(_) | StmtKind::Label(_) | StmtKind::Error => {}
        }
    }

    fn expr(&mut self, expr: &'a Expr, doc_anchor: Option<u32>) {
        match &expr.kind {
            ExprKind::Function(func) => self.func(func, doc_anchor, None),
            ExprKind::Call { callee, args, .. } => {
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
            _ => self.expr(expr, doc_anchor),
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
    /// The sets of values returned by the call of each `local a, b = f()` statement read so far,
    /// by the start of the statement.
    linked_sets: RefCell<FxHashMap<u32, Option<ReturnSets>>>,
    depth: Cell<u32>,
}

/// Natives call their handles `Vehicle`, `Ped` and so on. They are integers, and resources (ox_lib,
/// qbx_core) declare unrelated classes under the same names, so they must not resolve as classes.
pub(crate) const NATIVE_HANDLE_TYPES: &[&str] =
    &["Vehicle", "Ped", "Entity", "Object", "Player", "Hash", "Cam", "Blip", "Pickup", "ScrHandle", "FireId"];

fn native_type(name: &str) -> Type {
    if NATIVE_HANDLE_TYPES.contains(&name) {
        return Type::Handle(SmolStr::new(name));
    }
    Type::named(name)
}

pub fn native_fun_type(native: &qbx_fivem_data::Native) -> FunType {
    FunType {
        params: native
            .params()
            .map(|(name, ty)| Param { name: SmolStr::new(name), ty: native_type(ty), optional: false })
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
            linked_sets: RefCell::new(FxHashMap::default()),
            depth: Cell::new(0),
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
            return T::default();
        }
        self.depth.set(self.depth.get() + 1);
        let out = f();
        self.depth.set(self.depth.get() - 1);
        out
    }

    pub fn expr(&self, expr: &Expr) -> Type {
        self.expr_multi(expr).into_iter().next().unwrap_or_default()
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
    fn cast_after(&self, end: u32) -> Option<Type> {
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
            ExprKind::Field { base, name, .. } => {
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
            ExprKind::Index { base, index, .. } => self.index_expr(base, index),
            ExprKind::Table(fields) => self.table(fields),
            ExprKind::Binary { op, lhs, rhs, .. } => self.binary(*op, lhs, rhs),
            ExprKind::Unary { op, expr } => match op {
                UnOp::Not => Type::Boolean,
                UnOp::Len => match self.expr(expr) {
                    Type::Named(name, _) if name.starts_with("vector") => Type::Number,
                    _ => Type::Integer,
                },
                UnOp::BNot => Type::Integer,
                UnOp::Neg => self.expr(expr).widen(),
            },
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::Error => Type::Unknown,
        }
    }

    fn name(&self, name: &Name) -> Type {
        match self.ctx.resolution.resolve_at(name.span.start) {
            Some(Resolved::Local(id)) => self.local_type_at(id, name.span.start),
            _ => self.global_type(&name.text),
        }
    }

    /// The type of a local where it is read at `offset`: its own type without what the guards
    /// around the read rule out, such as the `nil` of a `string?` after `if not name then return end`.
    pub fn local_type_at(&self, id: LocalId, offset: u32) -> Type {
        self.narrowed(id, offset, self.local_type(id))
    }

    /// `ty`, the type of the local `id`, without what the guards around a read at `offset` rule out.
    pub fn narrowed(&self, id: LocalId, offset: u32, ty: Type) -> Type {
        let ty = self.linked_type(id, offset).unwrap_or(ty);
        let mut facts = self.ctx.guards().at(id, offset).peekable();
        if facts.peek().is_none() {
            return ty;
        }
        // An alias such as `Name = string|nil` is narrowed through what it stands for.
        let declared = match &ty {
            Type::Named(..) => self.resolve_alias(&ty),
            _ => ty.clone(),
        };
        // A guard that the type rules out altogether, like `if not name` for a `string`, is skipped.
        let narrowed = facts.fold(declared.clone(), |narrowed, fact| fact.apply(&narrowed).unwrap_or(narrowed));
        if narrowed == declared {
            ty
        } else {
            narrowed
        }
    }

    /// The type at `offset` of a local declared with others by one call, as `err` in
    /// `local ok, err = f()`, when the guards on those locals rule out some of the sets of values
    /// the call returns: what its position holds in the sets that are left.
    fn linked_type(&self, id: LocalId, offset: u32) -> Option<Type> {
        let guards = self.ctx.guards();
        let members = guards.linked(id)?;
        if !members.iter().any(|(member, _)| guards.at(*member, offset).next().is_some()) {
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
            Some(ty @ Type::Named(..)) => self.resolve_alias(ty),
            Some(ty) => ty.clone(),
            None => Type::Nil,
        };
        let is_possible = |set: &&Vec<Type>| {
            members.iter().all(|(member, position)| {
                let value = value(set, *position);
                guards.at(*member, offset).all(|fact| fact.apply(&value).is_some())
            })
        };
        let possible: Vec<&Vec<Type>> = sets.iter().filter(is_possible).collect();
        if possible.is_empty() || possible.len() == sets.len() {
            return None;
        }
        let position = members.iter().find(|(member, _)| *member == id)?.1;
        Some(merge_values(possible.iter().map(|set| set.get(position).cloned().unwrap_or(Type::Nil)).collect()))
    }

    /// The sets of values returned by the call that declares the locals of `stmt`, when its function
    /// lists several.
    fn linked_sets(&self, stmt: &Stmt) -> Option<ReturnSets> {
        if let Some(sets) = self.linked_sets.borrow().get(&stmt.span.start) {
            return sets.clone();
        }
        let StmtKind::Local { exprs, .. } = &stmt.kind else { return None };
        let sets = self.guarded(|| match exprs.last().map(|call| &call.kind) {
            Some(ExprKind::Call { callee, args, .. }) => self.call_values(callee, None, args).sets,
            Some(ExprKind::MethodCall { base, method, args, .. }) => self.call_values(base, Some(method), args).sets,
            _ => Vec::new(),
        });
        let sets = (sets.len() > 1).then(|| Rc::new(sets));
        self.linked_sets.borrow_mut().insert(stmt.span.start, sets.clone());
        sets
    }

    pub fn global_type(&self, name: &str) -> Type {
        if name == "exports" {
            return Type::Exports(None);
        }
        let symbols = self.index.globals_named(name, self.ctx.file);
        // `Zone = nil` declares a global that some handler sets later, not what it holds.
        let known = symbols
            .iter()
            .map(|(_, s)| &s.ty)
            .filter(|ty| !ty.is_unknown() && !matches!(ty, Type::Nil))
            .max_by_key(|ty| (matches!(ty, Type::GlobalTable(_) | Type::Named(..)), ty.specificity()));
        if let Some(ty) = known.filter(|ty| !(matches!(ty, Type::Table) && self.index.has_members(name))) {
            // `local lib = {}` published with `_ENV.lib = lib` and then extended as `function lib.x()`
            // elsewhere keeps its members under two owners.
            let aliased = matches!(ty, Type::GlobalTable(owner) if owner != name) && self.index.has_members(name);
            return if aliased { Type::union([ty.clone(), Type::GlobalTable(SmolStr::new(name))]) } else { ty.clone() };
        }
        if self.index.has_members(name) {
            return Type::GlobalTable(SmolStr::new(name));
        }
        match native(name) {
            Some(native) => Type::Fun(Arc::new(native_fun_type(&native))),
            None => Type::Unknown,
        }
    }

    pub fn local_type(&self, id: LocalId) -> Type {
        if let Some(ty) = self.locals.borrow().get(&id) {
            return ty.clone();
        }
        if !self.in_progress.borrow_mut().insert(id) {
            return Type::Unknown;
        }
        let ty = self.guarded(|| self.compute_local(id));
        self.in_progress.borrow_mut().remove(&id);
        self.locals.borrow_mut().insert(id, ty.clone());
        ty
    }

    fn compute_local(&self, id: LocalId) -> Type {
        let local = self.ctx.resolution.local(id);
        if local.kind == LocalKind::ImplicitSelf {
            return match self.ctx.decl(local.decl.start) {
                Some(Decl::SelfParam { name }) => self.func_name_owner_type(name),
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
            Decl::SelfParam { name } => self.func_name_owner_type(name),
            Decl::NumericFor => Type::Number,
            Decl::GenericFor { stmt, index } => self.for_in_type(stmt, *index),
        }
    }

    /// The type of the name at `index` of a `local` statement. A name that takes what a call returns
    /// has the type the call returns, unless it is `reassigned` after its declaration: it may then
    /// hold other values of that kind, so `"active"|"busy"` becomes `string`.
    fn local_stmt_type(&self, stmt: &Stmt, index: usize, top_level: bool, reassigned: bool) -> Type {
        let StmtKind::Local { names, exprs, in_unpack } = &stmt.kind else { return Type::Unknown };
        let doc = self.ctx.doc_at(stmt.span.start);
        if let Some(class) = doc.classes.last() {
            return Type::Named(class.name.clone(), Vec::new());
        }
        if let Some(ty) = doc.type_at(index) {
            return ty.clone();
        }
        if *in_unpack {
            let base = exprs.first().map(|e| self.expr(e)).unwrap_or_default();
            return self.member(&base, &names[index].name.text).map(|m| m.ty).unwrap_or_default();
        }
        if let Some(expr) = exprs.get(index) {
            let is_last = index + 1 == exprs.len();
            if top_level && table_fields(expr).is_some() {
                return Type::GlobalTable(self.ctx.local_owner_key(names[index].name.span.start));
            }
            let ty =
                if is_last { self.expr_multi(expr).into_iter().next().unwrap_or_default() } else { self.expr(expr) };
            // The `false|string` of a call keeps its `false`, while `local done = false` is a boolean.
            return match (expr.is_call(), reassigned) {
                (true, false) => ty,
                (true, true) => ty.widen_returned(),
                (false, _) => ty.widen(),
            };
        }
        match exprs.last() {
            Some(last) if last.is_multi_value() => {
                let ty = self.expr_multi(last).into_iter().nth(index + 1 - exprs.len()).unwrap_or_default();
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

    /// The function type that a function literal written where `expected` says has to be.
    pub fn expected_fun(&self, expected: &Expected) -> Option<Arc<FunType>> {
        self.fun_of(&self.expected_type(expected)?)
    }

    /// The type that a value written where `expected` says has to be, when something declares it.
    fn expected_type(&self, expected: &Expected) -> Option<Type> {
        match *expected {
            Expected::Arg { call, arg_index } => {
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
                // `---@class Name` above a table declares the class rather than a value of it.
                let doc = self.ctx.doc_at(stmt.span.start);
                doc.type_at(index).filter(|_| doc.classes.is_empty()).cloned()
            }
            Expected::Field { table, key } => {
                let at = self.ctx.tables.get(&table.span.start)?;
                let table = self.guarded(|| self.expected_type(at))?;
                self.field_type(&table, key)
            }
        }
    }

    /// The signature that `call` uses, with its arguments and whether it calls a method.
    fn call_parts<'e>(&self, call: &'e Expr) -> Option<(Arc<FunType>, CallArgs<'e>, bool)> {
        let (fun, args, via_method) = match &call.kind {
            ExprKind::Call { callee, args, .. } => (self.expr(callee).as_fun().cloned(), args, false),
            ExprKind::MethodCall { base, method, args, .. } => {
                let member = self.member(&self.expr(base), &method.text);
                (member.and_then(|m| m.ty.as_fun().cloned()), args, true)
            }
            _ => return None,
        };
        let args = CallArgs::new(args);
        let fun = self.signature_for(&fun?, &args, via_method, call.span.start);
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
    fn fun_of(&self, ty: &Type) -> Option<Arc<FunType>> {
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
                Some(Decl::Local { stmt, .. }) => !self.ctx.doc_at(stmt.span.start).classes.is_empty(),
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
                let defines = |expr: &Expr| matches!(&expr.kind, ExprKind::Function(f) if std::ptr::eq(&**f, func));
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

    fn for_in_type(&self, stmt: &Stmt, index: usize) -> Type {
        let StmtKind::GenericFor { exprs, .. } = &stmt.kind else { return Type::Unknown };
        let Some(first) = exprs.first() else { return Type::Unknown };
        let iterated = match &first.kind {
            ExprKind::Call { callee, args, .. } => match (callee.dotted_path().as_deref(), args.first()) {
                (Some(iterator @ ("pairs" | "ipairs" | "next" | "each")), Some(arg)) => {
                    Some((arg, iterator == "ipairs"))
                }
                _ => None,
            },
            // `for k, v in next, t`
            _ if first.dotted_path().as_deref() == Some("next") => exprs.get(1).map(|arg| (arg, false)),
            _ => None,
        };
        if let Some((arg, ipairs)) = iterated {
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
                let index = shape.index.iter().filter(|(key, _)| !array_only || is_integer_key(key)).cloned();
                fields.chain(array).chain(index).unzip()
            }
            Type::Named(ref name, _) => {
                let class = self.index.class(name, self.side).map(|(_, c)| c);
                let index = class
                    .and_then(|c| c.index(self.side))
                    .filter(|(key, _)| !array_only || is_integer_key(key))
                    .map(|(key, value)| (key.clone(), value.clone()));
                // `---@field [1] number` fields, with their keys shown as `integer` rather than `1|2|3`.
                let literal_fields = class
                    .into_iter()
                    .flat_map(|c| c.literal_fields(self.side))
                    .filter(|(key, _)| !array_only || is_integer_key(key))
                    .map(|(key, value)| (key.widen(), value.clone()));
                // An instance holds its data; the methods its class provides are not visited.
                let fields = self
                    .members(&resolved)
                    .into_iter()
                    .filter(|m| !array_only && !matches!(m.kind, SymbolKind::Method | SymbolKind::Function))
                    .map(|m| (Type::String, m.ty));
                let (keys, values): (Vec<Type>, Vec<Type>) = fields.chain(literal_fields).chain(index).unzip();
                if keys.is_empty() {
                    return (Type::String, Type::Unknown);
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

    /// Keys and values of a table the index holds: its named fields are members, its array part and
    /// other keys are elements. A top-level local of this file reads those from its constructor
    /// instead, which is current even while the index still has the file's previous text.
    fn global_table_key_values(&self, owner: &SmolStr, array_only: bool) -> (Type, Type) {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        if !array_only {
            for member in self.members(&Type::GlobalTable(owner.clone())) {
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
                Type::Named(name, _) if self.index.class(name, self.side).is_none() => {
                    match self.index.alias(name, self.side) {
                        Some((_, alias)) => current = alias.ty.clone(),
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

    fn module_type(&self, path: &str) -> Option<Type> {
        let file = self.index.resolve_require(path, self.ctx.file)?;
        self.index.file(file)?.index.module_return.clone()
    }

    fn index_expr(&self, base: &Expr, index: &Expr) -> Type {
        let base_ty = self.expr(base);
        let key_ty = self.expr(index);
        let mut keys = Vec::new();
        if self.literal_keys(&key_ty, 0, &mut keys) && !keys.is_empty() {
            let fields: Option<Vec<Type>> = keys.iter().map(|key| self.member(&base_ty, key).map(|m| m.ty)).collect();
            if let Some(fields) = fields {
                return Type::union(fields);
            }
        }
        self.element_type(&base_ty.without_nil(), &key_ty, 0)
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
            Type::Shape(shape) => Type::union(shape.array.iter().chain(shape.index.as_ref().map(|(_, v)| v)).cloned()),
            Type::Named(name, _) => {
                let Some((_, class)) = self.index.class(&name, self.side) else { return Type::Unknown };
                let index = class.index(self.side).map(|(_, value)| value.clone());
                // `employee[1]` reads the `---@field [1] number` of its key, and `employee[i]` any
                // literal-keyed field that `i` may name.
                let key_ty = self.resolve_alias(key_ty);
                if key_ty.is_literal() {
                    let field = class.literal_fields(self.side).find(|(key, _)| **key == key_ty);
                    return field.map(|(_, value)| value.clone()).or(index).unwrap_or_default();
                }
                let fields = class.literal_fields(self.side).filter(|(key, _)| may_be_literal_key(&key_ty, key));
                Type::union(fields.map(|(_, value)| value.clone()).chain(index))
            }
            // `list[i]` on a table whose array part the index or a top-level local's constructor holds.
            Type::GlobalTable(owner)
                if matches!(key_ty.without_nil().widen(), Type::Integer | Type::Number | Type::Unknown) =>
            {
                self.global_table_key_values(&owner, true).1
            }
            Type::String | Type::StringLit(_) => Type::Unknown,
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
                TableField::Named { name, value } => (name.text.clone(), self.expr(value).widen()),
                TableField::Keyed { key: Expr { kind: ExprKind::String(name), .. }, value } => {
                    (name.clone(), self.expr(value).widen())
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
            shape.index = Some((Type::union(keys), Type::union(values)));
        }
        Type::Shape(Arc::new(shape))
    }

    fn binary(&self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Type {
        match op {
            BinOp::Concat => Type::String,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => Type::Boolean,
            // `a and b` is `b` whenever `a` holds a value, so an unknown `b` leaves it unknown.
            BinOp::And => match self.expr(rhs).widen() {
                rhs if rhs.is_unknown() => Type::Unknown,
                rhs => Type::union([rhs, Type::BooleanLit(false)]).widen(),
            },
            BinOp::Or => {
                let left = self.expr(lhs).without_nil().widen();
                let left = match left {
                    Type::Boolean => Type::Unknown,
                    other => other,
                };
                Type::union([left, self.expr(rhs).widen()])
            }
            BinOp::BAnd | BinOp::BOr | BinOp::BXor | BinOp::Shl | BinOp::Shr | BinOp::IDiv => Type::Integer,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod | BinOp::Pow => {
                let (left, right) = (self.expr(lhs).widen(), self.expr(rhs).widen());
                let is_vector = |t: &Type| matches!(t, Type::Named(n, _) if n.starts_with("vector") || n == "quat");
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

    /// The callee's function type, taking `base:method` lookups into account.
    pub fn callee_fun(&self, base: &Expr, method: Option<&Name>) -> Option<(Arc<FunType>, Option<MemberInfo>)> {
        match method {
            Some(method) => {
                let member = self.member(&self.expr(base), &method.text)?;
                Some((member.ty.as_fun()?.clone(), Some(member)))
            }
            None => {
                let ty = self.expr(base);
                if let Some(fun) = ty.as_fun() {
                    return Some((fun.clone(), None));
                }
                match self.resolve_alias(&ty) {
                    Type::Named(name, _) => {
                        let call = self.index.class(&name, self.side).and_then(|(_, c)| c.call.clone());
                        call.filter(|f| applies_on(f.side, self.side)).map(|f| (f, None))
                    }
                    _ => None,
                }
            }
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

    /// What calling `base`, or its `method`, with `args` returns.
    fn call_values(&self, base: &Expr, method: Option<&Name>, args: &[Expr]) -> Returned {
        let only = |ty: Type, declared: bool| Returned { values: vec![ty], sets: Vec::new(), declared };
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
                (Some("setmetatable"), Some(arg)) => return only(self.expr(arg), false),
                (Some("tostring"), _) => return only(Type::String, true),
                (Some("tonumber"), _) => return only(Type::Number.optional(), true),
                _ => {}
            }
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
                    return Returned {
                        values: handler.returns.clone(),
                        sets: handler.return_sets.clone(),
                        declared: !handler.returns_inferred,
                    };
                }
            }
        }
        let generics = self.bind_generics(&fun, &args, method.is_some(), true);
        let bound = |types: &Vec<Type>| types.iter().map(|ret| substitute(ret, &generics)).collect();
        Returned {
            values: bound(&fun.returns),
            sets: fun.return_sets.iter().map(bound).collect(),
            declared: !fun.returns_inferred && fun.generics.is_empty(),
        }
    }

    /// The definition that a call of the global function `base` uses when client and server files
    /// define it differently, as a `shared_script` sees two `GetJob`s: the one left for the side the
    /// call runs on, if its arguments fit it. `Some(None)` when no single definition is left or the
    /// arguments do not fit it, and `None` when the sides do not split the global.
    fn sided_definition(&self, base: &Expr, method: Option<&Name>, args: &CallArgs) -> Option<Option<Arc<FunType>>> {
        let (None, ExprKind::Name(name)) = (method, &base.kind) else { return None };
        if !matches!(self.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_))) {
            return None;
        }
        let side_of = |file: FileId| self.index.file(file).and_then(|f| f.side);
        let defined: Vec<(Option<Side>, &Arc<FunType>)> = self
            .index
            .globals_named(&name.text, self.ctx.file)
            .into_iter()
            .filter_map(|(file, symbol)| Some((side_of(file), symbol.ty.as_fun()?)))
            .collect();
        let defined_on = |side| defined.iter().any(|(on, _)| *on == Some(side));
        if !defined_on(Side::Client) || !defined_on(Side::Server) {
            return None;
        }
        let side = self.side_at(base.span.start);
        let mut left: Vec<&Arc<FunType>> = Vec::new();
        for (on, fun) in defined {
            if applies_on(on, side) && !left.contains(&fun) {
                left.push(fun);
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
            self.unify(fun, &param.ty, &args.ty(self, *i).widen(), &mut bound, 0);
        }
        if with_callbacks {
            for (param, i) in bindable.iter().filter(|(_, i)| is_callback(i)) {
                self.unify(fun, &param.ty, args.ty(self, *i), &mut bound, 0);
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

    /// Matches a parameter type against the type of its argument, binding the generics in it.
    fn unify(&self, fun: &FunType, param: &Type, arg: &Type, bound: &mut Vec<(SmolStr, Type)>, depth: u32) {
        if depth > 8 || arg.is_unknown() {
            return;
        }
        match param {
            Type::Named(name, args) if args.is_empty() && self.is_generic(fun, name) => {
                if !bound.iter().any(|(n, _)| n == name) {
                    bound.push((name.clone(), arg.clone()));
                }
            }
            Type::Array(inner) => self.unify(fun, inner, &self.key_value_types(arg, true).1, bound, depth + 1),
            Type::Map(key, value) => {
                let (arg_key, arg_value) = self.key_value_types(arg, false);
                self.unify(fun, key, &arg_key, bound, depth + 1);
                self.unify(fun, value, &arg_value, bound, depth + 1);
            }
            Type::Union(types) => {
                for part in types.iter().filter(|t| !matches!(t, Type::Nil)) {
                    self.unify(fun, part, &arg.without_nil(), bound, depth + 1);
                }
            }
            Type::Fun(expected) => {
                if let Some(given) = arg.as_fun() {
                    for (want, got) in expected.returns.iter().zip(&given.returns) {
                        self.unify(fun, want, got, bound, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }

    /// Whether an argument passed for `param` can bind a generic of `fun`, as far as `unify` looks.
    fn can_bind(&self, fun: &FunType, param: &Type) -> bool {
        match param {
            Type::Named(name, args) => args.is_empty() && self.is_generic(fun, name),
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
                    (Some(given), self.value_kinds(fun, given, 0))
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
                let wanted = self.value_kinds(fun, param, depth + 1)?;
                let given = self.value_kinds(fun, arg, 0)?;
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
        self.value_kinds(fun, &param.ty, 0).map(|kinds| if param.optional { kinds | kind::NIL } else { kinds })
    }

    /// The kinds of Lua value `ty` allows, as a set of `kind` bits, or `None` when it allows any.
    fn value_kinds(&self, fun: &FunType, ty: &Type, depth: u32) -> Option<u8> {
        if depth > 8 {
            return None;
        }
        Some(match ty {
            Type::Unknown | Type::Any => return None,
            Type::Nil => kind::NIL,
            Type::Boolean | Type::BooleanLit(_) => kind::BOOLEAN,
            Type::Number | Type::Integer | Type::IntLit(_) | Type::Handle(_) => kind::NUMBER,
            Type::String | Type::StringLit(_) => kind::STRING,
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
            Type::Variadic(inner) => return self.value_kinds(fun, inner, depth + 1),
            Type::Named(name, _) if self.is_generic(fun, name) || NATIVE_HANDLE_TYPES.contains(&name.as_str()) => {
                return None
            }
            // Classes describe tables, and also userdata such as `vector3`.
            Type::Named(name, _) if self.index.class(name, self.side).is_some() => kind::TABLE | kind::OTHER,
            Type::Named(..) => match self.resolve_alias(ty) {
                Type::Named(..) => return None,
                resolved => return self.value_kinds(fun, &resolved, depth + 1),
            },
            Type::Union(types) => {
                let mut kinds = 0;
                for part in types {
                    kinds |= self.value_kinds(fun, part, depth + 1)?;
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
            let (returns, sets) = self.inferred_returns(&func.body);
            if returns.iter().any(|t| !t.is_unknown()) {
                fun.returns = returns;
                fun.return_sets = sets;
                fun.returns_inferred = true;
            }
        }
        fun
    }

    /// What an undocumented function returns: at each position, the union of what every `return`
    /// passes there. A `return` with fewer values, and running past the end of the body, give `nil`.
    /// With them come the sets of values its `return`s pass, when they differ and hold several
    /// values, as `(false)` and `(string, string)` do.
    fn inferred_returns(&self, body: &Block) -> (Vec<Type>, Vec<Vec<Type>>) {
        let mut exits: Vec<&[Expr]> = return_stmts(body).into_iter().map(|(_, exprs)| exprs).collect();
        if exits.is_empty() {
            return Default::default();
        }
        if !always_exits(body) {
            exits.push(&[]);
        }
        let lists: Vec<Vec<Type>> = exits.iter().map(|exprs| self.return_values(exprs)).collect();
        let width = lists.iter().map(Vec::len).max().unwrap_or(0);
        let at = |i: usize| lists.iter().map(|values| values.get(i).cloned().unwrap_or(Type::Nil)).collect();
        let returns = (0..width).map(|i| merge_values(at(i)).widen_returned()).collect();
        let mut sets: Vec<Vec<Type>> = Vec::new();
        for list in &lists {
            if !sets.contains(list) {
                sets.push(list.clone());
            }
        }
        let linked = width > 1 && (2..=MAX_RETURN_SETS).contains(&sets.len());
        (returns, if linked { sets } else { Vec::new() })
    }

    /// The values one `return` passes, the last of them spread when it is a call or `...`. A plain
    /// `true` or `false` stays, so that the sets of values tell `return false` from `return name`.
    fn return_values(&self, exprs: &[Expr]) -> Vec<Type> {
        let returned = |ty: Type| match ty {
            Type::BooleanLit(_) => ty,
            other => other.widen_returned(),
        };
        let mut values = Vec::new();
        for (i, expr) in exprs.iter().enumerate() {
            if i + 1 == exprs.len() {
                values.extend(self.expr_multi(expr).into_iter().map(returned));
            } else {
                values.push(returned(self.expr(expr)));
            }
        }
        values
    }

    pub fn member(&self, ty: &Type, name: &str) -> Option<MemberInfo> {
        self.guarded(|| {
            let mut found = self.members_matching(ty, Some(name));
            let best = (0..found.len()).max_by_key(|i| (found[*i].ty.specificity(), std::cmp::Reverse(*i)))?;
            Some(found.swap_remove(best))
        })
    }

    pub fn members(&self, ty: &Type) -> Vec<MemberInfo> {
        let mut members = self.guarded(|| self.members_matching(ty, None));
        let mut seen = FxHashSet::default();
        members.retain(|m| seen.insert(m.name.clone()));
        members
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
                    });
                }
            }
            Type::Named(name, _) => self.class_members(name, filter, &mut out, 0),
            Type::GlobalTable(owner) => self.owner_members(owner, filter, &mut out),
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
                for (file, symbol) in self.index.exports_of(resource) {
                    if wanted(&symbol.name) && !declared.contains(&symbol.name) {
                        out.push(member_from_symbol(file, symbol));
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn owner_members(&self, owner: &str, filter: Option<&str>, out: &mut Vec<MemberInfo>) {
        for (file, symbol) in self.index.members_of(owner, self.ctx.file) {
            if filter.is_none_or(|f| f == symbol.name) {
                let mut member = member_from_symbol(file, symbol);
                if matches!(member.ty, Type::Table | Type::Unknown) {
                    let nested = format!("{owner}.{}", symbol.name);
                    if self.index.has_members(&nested) {
                        member.ty = Type::GlobalTable(SmolStr::new(nested));
                    }
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
                });
            }
        }
    }

    fn class_members(&self, name: &str, filter: Option<&str>, out: &mut Vec<MemberInfo>, depth: u32) {
        if depth > 8 {
            return;
        }
        let mut defs = self.index.class_defs(name);
        defs.retain(|(_, class)| applies_on(class.side, self.side));
        if defs.is_empty() {
            if let Some((_, alias)) = self.index.alias(name, self.side) {
                out.extend(self.guarded(|| self.members_matching(&alias.ty, filter)));
            }
            return;
        }
        for (file, class) in &defs {
            let sides = class.field_sides.iter().copied().chain(std::iter::repeat(None));
            for (field, side) in class.fields.iter().zip(sides) {
                if filter.is_none_or(|n| n == field.name) && applies_on(side, self.side) {
                    out.push(member_from_symbol(*file, field));
                }
            }
        }
        self.owner_members(name, filter, out);
        for (_, class) in defs {
            for parent in &class.parents {
                self.class_members(parent, filter, out, depth + 1);
            }
        }
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

fn member_from_symbol(file: FileId, symbol: &crate::index::Symbol) -> MemberInfo {
    MemberInfo {
        name: symbol.name.clone(),
        ty: symbol.ty.clone(),
        doc: symbol.doc.clone(),
        deprecated: symbol.deprecated,
        literal: symbol.literal.clone(),
        kind: symbol.kind,
        location: Some((file, symbol.range)),
    }
}

fn substitute(ty: &Type, generics: &[(SmolStr, Type)]) -> Type {
    if generics.is_empty() {
        return ty.clone();
    }
    match ty {
        Type::Named(name, args) if args.is_empty() => {
            generics.iter().find(|(n, _)| n == name).map_or_else(|| ty.clone(), |(_, bound)| bound.clone())
        }
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
                Type::union(parts)
            }
        }
        Type::Map(k, v) => Type::Map(Box::new(substitute(k, generics)), Box::new(substitute(v, generics))),
        Type::Fun(fun) => Type::Fun(Arc::new(FunType {
            params: fun.params.iter().map(|p| Param { ty: substitute(&p.ty, generics), ..p.clone() }).collect(),
            returns: fun.returns.iter().map(|t| substitute(t, generics)).collect(),
            return_sets: fun
                .return_sets
                .iter()
                .map(|set| set.iter().map(|t| substitute(t, generics)).collect())
                .collect(),
            is_method: fun.is_method,
            returns_inferred: fun.returns_inferred,
            ..FunType::default()
        })),
        Type::Shape(shape) => Type::Shape(Arc::new(Shape {
            fields: shape.fields.iter().map(|f| ShapeField { ty: substitute(&f.ty, generics), ..f.clone() }).collect(),
            array: shape.array.as_ref().map(|t| substitute(t, generics)),
            index: shape.index.as_ref().map(|(k, v)| (substitute(k, generics), substitute(v, generics))),
        })),
        other => other.clone(),
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

/// What one position holds across several lists of returned values.
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
