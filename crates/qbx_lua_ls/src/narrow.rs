//! Type guards: what a condition tells about the locals it tests, in the code that only runs when
//! it held or failed. `if not name then return end` leaves `name` holding a value for the rest of
//! its block, and `if name then ... end` for the branch. `---@cast` lines change the type of a local
//! from their line on.

use qbx_lua_analysis::scope::{LocalId, Resolution, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{CommentKind, NumberValue, Span, Token, TokenKind};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::infer::always_exits;
use crate::luacats::{parse_cast, CastEntry};
use crate::types::Type;

/// The names `type` gives: those of Lua, and those of the vectors, quaternions and matrices CfxLua
/// adds.
const TYPE_NAMES: [&str; 13] = [
    "nil", "boolean", "number", "string", "table", "function", "thread", "userdata", "vector2", "vector3", "vector4",
    "quat", "matrix",
];
/// The names `math.type` gives for numbers.
const NUMBER_NAMES: [&str; 2] = ["integer", "float"];

/// What a guard tells about the value of a local.
#[derive(Clone, Debug, PartialEq)]
pub enum Fact {
    /// It is neither `nil` nor `false`.
    Truthy,
    /// It is `nil` or `false`.
    Falsy,
    /// It equals this `nil`, `true`, `false`, string or integer.
    Is(Type),
    /// It differs from this `nil`, `true`, `false`, string or integer.
    IsNot(Type),
    /// `type` gives this name for it, or `math.type` gives `integer` or `float`.
    Kind(&'static str),
    /// `type` or `math.type` gives another name for it.
    NotKind(&'static str),
    /// The facts of one of these lists hold, as after `type(x) == 'string' or type(x) == 'number'`.
    AnyOf(Vec<Vec<Fact>>),
}

impl Fact {
    /// What is left of `ty` for a value the fact holds for, or `None` when no value of `ty`
    /// satisfies it. A type that says nothing about its values stays as it is. The aliases `ty`
    /// names are read as classes, so the caller expands them first.
    pub fn apply(&self, ty: &Type) -> Option<Type> {
        match ty {
            Type::Union(parts) => {
                let kept: Vec<Type> = parts.iter().filter_map(|part| self.apply(part)).collect();
                // The `unknown` of `string|unknown` may be any value the fact holds for.
                let keeps_unknown = kept.contains(&Type::Unknown);
                let ty = (!kept.is_empty()).then(|| Type::union(kept))?;
                Some(if keeps_unknown { ty.or_unknown() } else { ty })
            }
            _ => self.apply_part(ty),
        }
    }

    /// What the code the fact guards takes `ty` to be: what `apply` leaves of it, or, for a `type`
    /// check that `ty` has no value for, the kind it checks for. Such a check is how code handles
    /// values its annotations leave out.
    pub fn assume(&self, ty: &Type) -> Option<Type> {
        self.apply(ty).or_else(|| self.kind_type())
    }

    /// The type of the values a `type` check, or each of several, lets through.
    fn kind_type(&self) -> Option<Type> {
        match self {
            Fact::Kind("integer") => Some(Type::Integer),
            Fact::Kind("float") => Some(Type::Number),
            Fact::Kind(name) => Some(Type::named(name)),
            Fact::AnyOf(alternatives) => {
                let kinds: Option<Vec<Type>> =
                    alternatives.iter().map(|facts| facts.iter().rev().find_map(Fact::kind_type)).collect();
                kinds.map(Type::union)
            }
            _ => None,
        }
    }

    fn apply_part(&self, ty: &Type) -> Option<Type> {
        let unknown = matches!(ty, Type::Unknown | Type::Any);
        match self {
            Fact::AnyOf(alternatives) => {
                let kept: Vec<Type> = alternatives
                    .iter()
                    .filter_map(|facts| facts.iter().try_fold(ty.clone(), |ty, fact| fact.apply(&ty)))
                    .collect();
                (!kept.is_empty()).then(|| Type::union(kept))
            }
            Fact::Kind(_) if unknown => self.kind_type(),
            Fact::Kind(name) => match is_kind(ty, name) {
                Some(false) => None,
                None if *name == "integer" && *ty == Type::Number => Some(Type::Integer),
                _ => Some(ty.clone()),
            },
            Fact::NotKind(name) => (is_kind(ty, name) != Some(true)).then(|| ty.clone()),
            Fact::Is(value) if unknown => Some(value.clone()),
            _ if unknown => Some(ty.clone()),
            _ => match (ty, self) {
                (Type::Boolean, Fact::Truthy) => Some(Type::BooleanLit(true)),
                (Type::Boolean, Fact::Falsy) => Some(Type::BooleanLit(false)),
                (Type::Boolean, Fact::Is(Type::BooleanLit(value))) => Some(Type::BooleanLit(*value)),
                (Type::Boolean, Fact::IsNot(Type::BooleanLit(value))) => Some(Type::BooleanLit(!*value)),
                (Type::Boolean, Fact::Is(_)) => None,
                (Type::Boolean, _) => Some(Type::Boolean),
                // A string or number equal to a literal is that literal. A handle keeps its name.
                (Type::String, Fact::Is(value @ Type::StringLit(_)))
                | (Type::Integer | Type::Number, Fact::Is(value @ Type::IntLit(_))) => Some(value.clone()),
                (Type::Handle(_), Fact::Is(Type::IntLit(_))) => Some(ty.clone()),
                _ => {
                    let falsy = matches!(ty, Type::Nil | Type::BooleanLit(false));
                    let holds = match self {
                        Fact::Truthy => !falsy,
                        Fact::Falsy => falsy,
                        Fact::Is(value) => ty == value,
                        Fact::IsNot(value) => ty != value,
                        _ => true,
                    };
                    holds.then(|| ty.clone())
                }
            },
        }
    }
}

/// Whether `type`, or for `integer` and `float` `math.type`, gives `name` for every value of `ty`
/// (`Some(true)`) or for none of them (`Some(false)`). `None` when it may give it for some, or the
/// kind of `ty` is not known.
fn is_kind(ty: &Type, name: &str) -> Option<bool> {
    let integer = matches!(ty, Type::Integer | Type::IntLit(_) | Type::Handle(_));
    match name {
        "integer" | "float" if integer => Some(name == "integer"),
        "integer" | "float" if *ty == Type::Number => None,
        "integer" | "float" => type_name(ty).map(|_| false),
        _ => type_name(ty).map(|kind| kind == name),
    }
}

/// The name `type` gives for the values of `ty`.
fn type_name(ty: &Type) -> Option<&str> {
    Some(match ty {
        Type::Nil => "nil",
        Type::Boolean | Type::BooleanLit(_) => "boolean",
        Type::Number | Type::Integer | Type::IntLit(_) | Type::Handle(_) => "number",
        Type::String | Type::StringLit(_) => "string",
        Type::Table
        | Type::Array(_)
        | Type::Map(..)
        | Type::Tuple(_)
        | Type::Shape(_)
        | Type::GlobalTable(_)
        | Type::Exports(_) => "table",
        Type::Function | Type::Fun(_) => "function",
        Type::Thread => "thread",
        Type::Userdata => "userdata",
        // Classes describe tables, except for the vectors, quaternions and matrices of CfxLua.
        Type::Named(name, _) => match name.as_str() {
            name @ ("vector2" | "vector3" | "vector4" | "quat" | "matrix") => name,
            _ => "table",
        },
        _ => return None,
    })
}

type Facts = Vec<(LocalId, Fact)>;

/// The facts the guards of a file establish, each for a local and the code it holds in.
#[derive(Debug, Default)]
pub struct Guards {
    facts: FxHashMap<LocalId, Vec<(Span, Fact)>>,
    /// The locals with guards that are assigned again after their declaration.
    reassigned: FxHashSet<LocalId>,
    /// The locals that one call declares together, as in `local ok, err = f()`, each with the
    /// position of its value among those the call returns.
    links: Vec<Vec<(LocalId, usize)>>,
    link_of: FxHashMap<LocalId, usize>,
}

impl Guards {
    pub fn of(chunk: &Chunk, resolution: &Resolution) -> Self {
        let mut finder = Finder {
            resolution,
            guards: Guards::default(),
            kinds: FxHashMap::default(),
            type_functions: FxHashMap::default(),
        };
        finder.visit_block(&chunk.block);
        let mut guards = finder.guards;
        let reassigned = guards.facts.keys().filter(|local| resolution.local(**local).refs.iter().any(|r| r.write));
        guards.reassigned = reassigned.copied().collect();
        guards
    }

    /// What the guards around `offset` tell about `local`.
    pub fn at(&self, local: LocalId, offset: u32) -> impl Iterator<Item = &Fact> {
        self.since(local, None, Span::empty(offset))
    }

    /// What the guards that hold from the start to the end of `code` tell about `local`, with the
    /// start of a `cast` that holds there only those that take effect at its start or later. A
    /// guard on a local that is assigned again only tells about the type a cast gives it: the value
    /// it tested may have been replaced since, unless a cast that holds until the local is next
    /// assigned started before the guard.
    pub fn since(&self, local: LocalId, cast: Option<u32>, code: Span) -> impl Iterator<Item = &Fact> {
        let facts = match cast {
            None if self.reassigned.contains(&local) => &[],
            _ => self.facts.get(&local).map(Vec::as_slice).unwrap_or_default(),
        };
        let start = cast.unwrap_or(0);
        let holds = move |span: &Span| start <= span.start && span.contains(code.start) && span.contains(code.end);
        facts.iter().filter(move |(span, _)| holds(span)).map(|(_, fact)| fact)
    }

    /// The locals declared by the same call as `local`, itself included, each with the position of
    /// its value among those the call returns. A guard on one of them also tells which of the sets
    /// of values the call can return the others come from.
    pub fn linked(&self, local: LocalId) -> Option<&[(LocalId, usize)]> {
        self.link_of.get(&local).map(|link| self.links[*link].as_slice())
    }
}

struct Finder<'r> {
    resolution: &'r Resolution,
    guards: Guards,
    /// The locals that hold what `type`, or `math.type` with `true`, gives for another local, as
    /// `local kind = type(value)` does.
    kinds: FxHashMap<LocalId, (LocalId, bool)>,
    /// The locals that hold `type`, or with `true` `math.type`, as after `local type = type`.
    type_functions: FxHashMap<LocalId, bool>,
}

impl Finder<'_> {
    /// The local `expr` names. A guard on one that is assigned again after its declaration only
    /// counts after a `---@cast`, see `Guards::since`.
    fn local(&self, expr: &Expr) -> Option<LocalId> {
        let ExprKind::Name(name) = &expr.unparen().kind else { return None };
        let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.span.start) else { return None };
        Some(id)
    }

    /// The local that the fields, indexes and calls of `expr` are read from, as `data` in
    /// `data?.job.name` and `data:get()`. A value read through it means that it holds one.
    fn read_from(&self, expr: &Expr) -> Option<LocalId> {
        let expr = expr.unparen();
        let base = match &expr.kind {
            ExprKind::Field { base, .. } | ExprKind::Index { base, .. } | ExprKind::MethodCall { base, .. } => base,
            ExprKind::Call { callee, .. } if !matches!(callee.kind, ExprKind::Name(_)) => callee,
            _ => return None,
        };
        match &base.unparen().kind {
            ExprKind::Name(_) => self.local(base),
            _ => self.read_from(base),
        }
    }

    fn is_reassigned(&self, local: LocalId) -> bool {
        self.resolution.local(local).refs.iter().any(|r| r.write)
    }

    fn is_global(&self, name: &Name) -> bool {
        !matches!(self.resolution.resolve_at(name.span.start), Some(Resolved::Local(_)))
    }

    /// The local that a `type(value)` call checks, or with `true` a `math.type(value)` call.
    fn type_call(&self, expr: &Expr) -> Option<(LocalId, bool)> {
        let ExprKind::Call { callee, args, .. } = &expr.unparen().kind else { return None };
        let [arg] = args.as_slice() else { return None };
        let numbers = self.type_function(callee)?;
        Some((self.local(arg)?, numbers))
    }

    /// Whether `expr` is the global `type` (`false`) or `math.type` (`true`), or a local that holds
    /// one of them, as after `local type = type`.
    fn type_function(&self, expr: &Expr) -> Option<bool> {
        match &expr.kind {
            ExprKind::Name(name) => match self.resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => self.type_functions.get(&id).copied(),
                _ => (name.text == "type").then_some(false),
            },
            ExprKind::Field { base, name, .. } if name.text == "type" => match &base.kind {
                ExprKind::Name(math) if math.text == "math" && self.is_global(math) => Some(true),
                _ => None,
            },
            _ => None,
        }
    }

    /// The local whose kind `side` gives and the name of a kind that `other` compares it with, as
    /// in `type(value) == 'table'`, `math.type(value) == 'integer'` and, after
    /// `local kind = type(value)`, `kind == 'table'`.
    fn kind_check(&self, side: &Expr, other: &Expr) -> Option<(LocalId, &'static str)> {
        let name = other.unparen().as_string()?;
        let (local, numbers) = match &side.unparen().kind {
            ExprKind::Name(_) => *self.kinds.get(&self.local(side)?)?,
            _ => self.type_call(side)?,
        };
        let names: &[&'static str] = if numbers { &NUMBER_NAMES } else { &TYPE_NAMES };
        names.iter().find(|kind| **kind == name.as_str()).map(|kind| (local, *kind))
    }

    /// Adds what `cond` being true, or false without `holds`, tells about the locals in it.
    fn facts(&self, cond: &Expr, holds: bool, out: &mut Facts) {
        let cond = cond.unparen();
        match &cond.kind {
            ExprKind::Name(_) => {
                if let Some(local) = self.local(cond) {
                    out.push((local, if holds { Fact::Truthy } else { Fact::Falsy }));
                }
            }
            ExprKind::Unary { op: UnOp::Not, expr } => self.facts(expr, !holds, out),
            // Both sides of a true `and` are true, and both sides of a false `or` are false.
            ExprKind::Binary { op: BinOp::And, lhs, rhs, .. } if holds => {
                self.facts(lhs, true, out);
                self.facts(rhs, true, out);
            }
            ExprKind::Binary { op: BinOp::Or, lhs, rhs, .. } if !holds => {
                self.facts(lhs, false, out);
                self.facts(rhs, false, out);
            }
            // One side of a true `or` is true, and one side of a false `and` is false.
            ExprKind::Binary { op: BinOp::Or | BinOp::And, lhs, rhs, .. } => self.either(lhs, rhs, holds, out),
            ExprKind::Binary { op: op @ (BinOp::Eq | BinOp::Ne), lhs, rhs, .. } => {
                let equal = (*op == BinOp::Eq) == holds;
                let compared = match (self.local(lhs), literal(rhs)) {
                    (Some(local), Some(value)) => Some((local, value)),
                    _ => self.local(rhs).zip(literal(lhs)),
                };
                if let Some((local, value)) = compared {
                    out.push((local, if equal { Fact::Is(value) } else { Fact::IsNot(value) }));
                }
                if let Some((local, kind)) = self.kind_check(lhs, rhs).or_else(|| self.kind_check(rhs, lhs)) {
                    out.push((local, if equal { Fact::Kind(kind) } else { Fact::NotKind(kind) }));
                }
                // `data?.job == 'police'` and `data?.job ~= nil` read a value from `data`.
                for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                    if let (Some(local), Some(value)) = (self.read_from(side), literal(other)) {
                        if equal != (value == Type::Nil) {
                            out.push((local, Fact::Truthy));
                        }
                    }
                }
            }
            // `data?.job`, `data.job`, `data[key]` and `data:get()` are only true when `data` holds a
            // value: `?.` gives `nil` for a `nil` one, and the others raise an error.
            _ if holds => {
                if let Some(local) = self.read_from(cond) {
                    out.push((local, Fact::Truthy));
                }
            }
            _ => {}
        }
    }

    /// Adds what one of `lhs` and `rhs` being true, or false without `holds`, tells: for each local
    /// that both tell something about, that the facts of one of them hold.
    fn either(&self, lhs: &Expr, rhs: &Expr, holds: bool, out: &mut Facts) {
        let (mut left, mut right) = (Facts::new(), Facts::new());
        self.facts(lhs, holds, &mut left);
        self.facts(rhs, holds, &mut right);
        let about = |facts: &Facts, local: LocalId| -> Vec<Fact> {
            facts.iter().filter(|(of, _)| *of == local).map(|(_, fact)| fact.clone()).collect()
        };
        let mut locals: Vec<LocalId> = Vec::new();
        for (local, _) in &left {
            if !locals.contains(local) {
                locals.push(*local);
            }
        }
        for local in locals {
            let second = about(&right, local);
            if !second.is_empty() {
                out.push((local, Fact::AnyOf(vec![about(&left, local), second])));
            }
        }
    }

    /// Links the locals that take the values of the call ending a `local` statement. Those assigned
    /// again later are left out, as they are not narrowed.
    fn link(&mut self, names: &[AttribName], exprs: &[Expr]) {
        let Some(first) = exprs.len().checked_sub(1).filter(|_| exprs.last().is_some_and(Expr::is_call)) else {
            return;
        };
        let members: Vec<(LocalId, usize)> = names
            .iter()
            .enumerate()
            .skip(first)
            .filter_map(|(index, name)| {
                let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.name.span.start) else { return None };
                (!self.resolution.local(id).refs.iter().any(|r| r.write)).then_some((id, index - first))
            })
            .collect();
        if members.len() > 1 {
            for (local, _) in &members {
                self.guards.link_of.insert(*local, self.guards.links.len());
            }
            self.guards.links.push(members);
        }
    }

    fn record(&mut self, span: Span, facts: Facts) {
        for (local, fact) in facts {
            self.guards.facts.entry(local).or_default().push((span, fact));
        }
    }

    /// What holds once `stmt` has run: the conditions of an `if` whose other paths all leave, as
    /// after `if not name then return end`, and the condition of an `assert`.
    fn after(&self, stmt: &Stmt) -> Facts {
        match &stmt.kind {
            StmtKind::If { branches, else_block } => {
                // The ways through the statement that reach the code after it.
                let mut open: Vec<Facts> = Vec::new();
                let mut failed = Facts::new();
                for branch in branches {
                    if !leaves(&branch.block) {
                        let mut facts = failed.clone();
                        self.facts(&branch.cond, true, &mut facts);
                        open.push(facts);
                    }
                    self.facts(&branch.cond, false, &mut failed);
                }
                if !else_block.as_ref().is_some_and(leaves) {
                    open.push(failed);
                }
                match open.len() {
                    1 => open.pop().unwrap_or_default(),
                    _ => Facts::new(),
                }
            }
            StmtKind::Expr(Expr { kind: ExprKind::Call { callee, args, .. }, .. }) => {
                let is_assert = matches!(&callee.kind, ExprKind::Name(name) if name.text == "assert"
                    && !matches!(self.resolution.resolve_at(name.span.start), Some(Resolved::Local(_))));
                let mut facts = Facts::new();
                if let Some(cond) = args.first().filter(|_| is_assert) {
                    self.facts(cond, true, &mut facts);
                }
                facts
            }
            _ => Facts::new(),
        }
    }
}

impl<'ast> Visitor<'ast> for Finder<'_> {
    fn visit_block(&mut self, block: &'ast Block) {
        for (index, stmt) in block.stmts.iter().enumerate() {
            self.visit_stmt(stmt);
            let facts = self.after(stmt);
            if !facts.is_empty() {
                // A label can be reached by a `goto` that skipped the guard.
                let label = block.stmts[index + 1..].iter().find(|next| matches!(next.kind, StmtKind::Label(_)));
                let end = label.map_or(block.span.end, |label| label.span.start);
                self.record(Span { start: stmt.span.end, end: end.max(stmt.span.end) }, facts);
            }
        }
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::If { branches, else_block } => {
                let else_start = else_block.as_ref().map(|block| block.span.start.min(stmt.span.end));
                let mut failed = Facts::new();
                for (index, branch) in branches.iter().enumerate() {
                    let mut facts = failed.clone();
                    self.facts(&branch.cond, true, &mut facts);
                    // Measured between the keywords so half-typed code inside the branch still counts.
                    let next = branches.get(index + 1).map(|b| b.keyword_span.start);
                    let end = next.or(else_start).unwrap_or(stmt.span.end);
                    self.record(Span { start: branch.cond.span.end, end: end.max(branch.cond.span.end) }, facts);
                    self.facts(&branch.cond, false, &mut failed);
                }
                if let Some(start) = else_start {
                    self.record(Span { start, end: stmt.span.end }, failed);
                }
            }
            StmtKind::Local { names, exprs, in_unpack: false } => {
                self.link(names, exprs);
                for (name, expr) in names.iter().zip(exprs) {
                    let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.name.span.start) else { continue };
                    if self.is_reassigned(id) {
                        continue;
                    }
                    // `local kind = type(value)` tells nothing about what is assigned to `value` later.
                    if let Some(checked) = self.type_call(expr).filter(|(checked, _)| !self.is_reassigned(*checked)) {
                        self.kinds.insert(id, checked);
                    } else if let Some(numbers) = self.type_function(expr) {
                        self.type_functions.insert(id, numbers);
                    }
                }
            }
            StmtKind::While { cond, .. } => {
                let mut facts = Facts::new();
                self.facts(cond, true, &mut facts);
                self.record(Span { start: cond.span.end, end: stmt.span.end.max(cond.span.end) }, facts);
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // `b` of `a and b` only runs when `a` is true, and `b` of `a or b` when it is false.
        if let ExprKind::Binary { op: op @ (BinOp::And | BinOp::Or), lhs, rhs, .. } = &expr.kind {
            let mut facts = Facts::new();
            self.facts(lhs, *op == BinOp::And, &mut facts);
            self.record(rhs.span, facts);
        }
        visit::walk_expr(self, expr);
    }
}

fn literal(expr: &Expr) -> Option<Type> {
    match &expr.unparen().kind {
        ExprKind::Nil => Some(Type::Nil),
        ExprKind::True => Some(Type::BooleanLit(true)),
        ExprKind::False => Some(Type::BooleanLit(false)),
        ExprKind::String(value) => Some(Type::StringLit(value.clone())),
        ExprKind::Number(NumberValue::Int(value)) => Some(Type::IntLit(*value)),
        ExprKind::Unary { op: UnOp::Neg, expr } => match &expr.unparen().kind {
            ExprKind::Number(NumberValue::Int(value)) => value.checked_neg().map(Type::IntLit),
            _ => None,
        },
        _ => None,
    }
}

/// Whether running `block` always leaves the block around it: by returning, raising an error,
/// `break` or `goto`.
fn leaves(block: &Block) -> bool {
    match block.stmts.last().map(|stmt| &stmt.kind) {
        Some(StmtKind::Break) => true,
        Some(StmtKind::Do(body)) => leaves(body),
        Some(StmtKind::If { branches, else_block: Some(else_block) }) => {
            branches.iter().all(|branch| leaves(&branch.block)) && leaves(else_block)
        }
        _ => always_exits(block),
    }
}

/// A `---@cast` line.
#[derive(Debug)]
pub struct Cast {
    /// The local it names, when one by that name is in scope.
    pub local: Option<LocalId>,
    /// What each entry does, with where its type is written.
    pub entries: Vec<(CastEntry, Span)>,
    /// The code it holds in: from its line to the end of the block the statement after it is in,
    /// or to the end of the statement that next assigns the local. One on the last line of an `if`
    /// branch holds from the end of the `if`, and one on the last line of another block nowhere.
    pub span: Span,
}

/// The `---@cast` lines of a file.
#[derive(Debug, Default)]
pub struct Casts {
    casts: Vec<Cast>,
}

impl Casts {
    pub fn of(source: &str, chunk: &Chunk, resolution: &Resolution) -> Self {
        let mut casts = Vec::new();
        for comment in chunk.comments.iter().filter(|comment| comment.kind == CommentKind::Line) {
            let text = comment.content.text(source);
            let Some(line) = text.strip_prefix('-').map(str::trim_start) else { continue };
            let Some(cast) = parse_cast(line) else { continue };
            // A line without a type, as while one is being typed, changes nothing.
            if cast.entries.is_empty() {
                continue;
            }
            let base = comment.content.end - line.len() as u32;
            let entries = cast.entries.into_iter();
            casts.push(Cast {
                local: resolution.lookup_local_at(cast.name, comment.span.start),
                entries: entries
                    .map(|(entry, at)| (entry, Span::new(base + at.start as u32, base + at.end as u32)))
                    .collect(),
                span: Span::empty(comment.span.end),
            });
        }
        if casts.is_empty() {
            return Self::default();
        }
        let mut locals: Vec<LocalId> = casts.iter().filter_map(|cast| cast.local).collect();
        locals.sort_unstable();
        locals.dedup();
        let writes = |local: LocalId| resolution.local(local).refs.iter().filter(|r| r.write).map(|r| r.span.start);
        let mut reach = Reach {
            tokens: &chunk.tokens,
            statements: Vec::new(),
            branch_ends: FxHashMap::default(),
            writes: locals.iter().flat_map(|local| writes(*local)).map(|at| (at, u32::MAX)).collect(),
        };
        reach.writes.sort_unstable();
        reach.visit_block(&chunk.block);
        reach.statements.sort_unstable();
        // For each local, where each write to it starts, with the earliest end of a statement that
        // assigns it from that write on.
        let mut assigned = FxHashMap::default();
        for local in locals {
            let mut ends: Vec<(u32, u32)> = writes(local).map(|at| (at, reach.end_of_write(at))).collect();
            ends.sort_unstable();
            let mut earliest = u32::MAX;
            for (_, end) in ends.iter_mut().rev() {
                earliest = earliest.min(*end);
                *end = earliest;
            }
            assigned.insert(local, ends);
        }
        for cast in &mut casts {
            let line_end = cast.span.start;
            let Some((start, end)) = reach.from(line_end) else { continue };
            let reassigned = cast.local.and_then(|local| {
                let ends: &Vec<(u32, u32)> = &assigned[&local];
                ends.get(ends.partition_point(|(at, _)| *at <= line_end)).map(|(_, end)| *end)
            });
            cast.span = Span::new(start, end.min(reassigned.unwrap_or(u32::MAX)).max(start));
        }
        Self { casts }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Cast> {
        self.casts.iter()
    }

    /// The casts of `local` that hold at `offset`, in the order of their lines.
    pub fn at(&self, local: LocalId, offset: u32) -> impl Iterator<Item = &Cast> {
        self.casts.iter().filter(move |cast| cast.local == Some(local) && cast.span.contains(offset))
    }
}

/// Finds where the casts of a file stop holding.
struct Reach<'a> {
    tokens: &'a [Token],
    /// Where each statement starts, with the end of the block it is in.
    statements: Vec<(u32, u32)>,
    /// Where each `elseif`, `else` and `end` of an `if` starts, with the end of that `if`.
    branch_ends: FxHashMap<u32, u32>,
    /// Where each write to the local of a cast starts, in order, with the end of the innermost
    /// statement it is in.
    writes: Vec<(u32, u32)>,
}

impl Reach<'_> {
    /// Where a cast whose line ends at `at` starts holding, and the end of the block it holds to:
    /// that of the statement after it. After the last statement of an `if` branch, a cast holds
    /// from the end of the `if`, as code types what the branch leaves that way, and after that of
    /// another block, such as a function or loop body, nowhere.
    fn from(&self, at: u32) -> Option<(u32, u32)> {
        let next = self.tokens.get(self.tokens.partition_point(|token| token.span.start < at))?;
        if let Some(end) = self.branch_ends.get(&next.span.start) {
            return self.from(*end);
        }
        if matches!(next.kind, TokenKind::End | TokenKind::Until | TokenKind::Eof) {
            return None;
        }
        let next = self.statements.partition_point(|(start, _)| *start < at);
        self.statements.get(next).map(|(_, end)| (at, *end))
    }

    /// The end of the innermost statement that the write starting at `at` is in.
    fn end_of_write(&self, at: u32) -> u32 {
        let index = self.writes.partition_point(|(start, _)| *start < at);
        self.writes.get(index).map_or(u32::MAX, |(_, end)| *end)
    }
}

impl<'ast> Visitor<'ast> for Reach<'_> {
    fn visit_block(&mut self, block: &'ast Block) {
        self.statements.extend(block.stmts.iter().map(|stmt| (stmt.span.start, block.span.end)));
        visit::walk_block(self, block);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // Statements are visited before those inside them, so the innermost one is seen last.
        let first = self.writes.partition_point(|(at, _)| *at < stmt.span.start);
        let after = self.writes.partition_point(|(at, _)| *at < stmt.span.end);
        for (_, end) in &mut self.writes[first..after] {
            *end = stmt.span.end;
        }
        if let StmtKind::If { branches, else_block } = &stmt.kind {
            let tokens = self.tokens;
            let before = |offset: u32| tokens[..tokens.partition_point(|token| token.span.start < offset)].last();
            let elseifs = branches.iter().skip(1).map(|branch| branch.keyword_span.start);
            let keyword = else_block.as_ref().and_then(|block| before(block.span.start));
            let keyword = keyword.filter(|token| token.kind == TokenKind::Else).map(|token| token.span.start);
            let end = before(stmt.span.end).filter(|token| token.kind == TokenKind::End);
            for start in elseifs.chain(keyword).chain(end.map(|token| token.span.start)) {
                self.branch_ends.insert(start, stmt.span.end);
            }
        }
        visit::walk_stmt(self, stmt);
    }
}
