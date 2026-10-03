//! `impossible-comparison`: an `==` or `~=` whose two sides can never be equal, so that it always
//! gives the same answer. The sides are different kinds of value, as a string and a number are, or
//! they list literals of which none is on both sides, as `"invalid"` is none of
//! `"active"|"busy"|"ready"`.
//!
//! A side only has a type that something declares: a literal, an operator, an annotation, a stub
//! or a native, also the callee of a call for the parameters of a function passed to it. What
//! inference reads from assigned values is left out, since `Config.Webhook = ''` tells what is
//! stored today and not that `false` never is. Comparisons with `nil` are left alone as well:
//! annotations often leave out the `?` of a value that may be missing, and the check for it is
//! deliberate.

use std::cell::{OnceCell, RefCell};

use qbx_fivem_data::native;
use qbx_lua_analysis::scope::{Local, LocalId, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;
use rustc_hash::{FxHashMap, FxHashSet};

use super::class_tables::{Classes, Key};
use super::unknown_types::has_param_line;
use crate::infer::{assigned_value, Decl, Expected, Infer};
use crate::narrow::Origin;
use crate::types::Type;

const MAX_DEPTH: u32 = 16;

/// Each `==` and `~=` of `chunk` whose sides share no value, with a message naming both types and
/// the answer the comparison always gives.
pub fn impossible_comparisons(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let mut finder = Finder { declared: Declared::new(infer), out: Vec::new() };
    finder.visit_block(&chunk.block);
    finder.out
}

struct Finder<'a, 'b> {
    declared: Declared<'a, 'b>,
    out: Vec<(Span, String)>,
}

/// Reads the types that expressions are declared to have.
pub struct Declared<'a, 'b> {
    infer: &'a Infer<'b>,
    classes: Classes<'a, 'b>,
    /// Whether a local that is assigned again keeps the type its annotation gives it.
    keeps_annotations: bool,
    /// Where the expression that each `--[[@as T]]` casts starts, by where it ends.
    cast_starts: OnceCell<FxHashMap<u32, u32>>,
    /// The types the locals read so far are declared with.
    declarations: RefCell<FxHashMap<LocalId, Type>>,
    /// The declared types of the values that the declarations and assignments of locals that are
    /// assigned again give them, by the local and the origin of the value.
    origins: RefCell<FxHashMap<(LocalId, u32), Type>>,
    in_progress: RefCell<FxHashSet<(LocalId, u32)>>,
}

impl<'a, 'b> Declared<'a, 'b> {
    pub fn new(infer: &'a Infer<'b>) -> Self {
        Self {
            infer,
            classes: Classes::new(infer),
            keeps_annotations: false,
            cast_starts: OnceCell::new(),
            declarations: RefCell::default(),
            origins: RefCell::default(),
            in_progress: RefCell::default(),
        }
    }

    /// Takes the `---@type` or `@param` of a local at its word, also where the local is assigned
    /// again. Completion reads the values to offer for it from there, which no diagnostic can
    /// rely on.
    pub fn keeping_annotations(infer: &'a Infer<'b>) -> Self {
        Self { keeps_annotations: true, ..Self::new(infer) }
    }

    /// The type of `expr` as far as it is declared, and `unknown` beyond that.
    pub fn of(&self, expr: &Expr) -> Type {
        self.declared(expr, 0)
    }

    fn declared(&self, expr: &Expr, depth: u32) -> Type {
        if depth > MAX_DEPTH {
            return Type::Unknown;
        }
        // `local node = self.tail --[[@as LruNode]]` declares what the field holds there.
        if let Some(cast) = self.cast(expr) {
            return cast;
        }
        let expr = expr.unparen();
        match &expr.kind {
            ExprKind::Name(name) => match self.infer.ctx.resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => self.local(id, name.span.start, depth),
                _ => self.global(&name.text),
            },
            ExprKind::Field { base, name, .. } => self.field(base, &name.text, depth),
            ExprKind::Index { base, index, .. } => match index.as_string() {
                Some(name) => self.field(base, name, depth),
                None => Type::Unknown,
            },
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                self.returned(expr).and_then(|values| values.into_iter().next()).unwrap_or_default()
            }
            // `-'5'` is a number, and a table with an `__unm` metamethod gives anything.
            ExprKind::Unary { op: UnOp::Neg, expr: operand } => match self.declared(operand, depth + 1).widen() {
                ty @ (Type::Number | Type::Integer) => ty,
                _ => Type::Unknown,
            },
            // `a or b` is either of them, and `a and b` may be what `a` holds.
            ExprKind::Binary { op: BinOp::And | BinOp::Or, .. } => Type::Unknown,
            // Vectors, and tables with metamethods, give other values than numbers.
            ExprKind::Binary {
                op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::IDiv | BinOp::Mod | BinOp::Pow,
                lhs,
                rhs,
                ..
            } => {
                let is_number = |operand: &Expr| {
                    let ty = self.declared(operand, depth + 1).widen();
                    matches!(ty, Type::Number | Type::Integer)
                };
                if is_number(lhs) && is_number(rhs) {
                    self.infer.expr(expr)
                } else {
                    Type::Unknown
                }
            }
            // Literals, and operators whose result is of one kind whatever their operands are.
            _ => self.infer.expr(expr),
        }
    }

    /// The type that a `--[[@as T]]` after `expr` casts it to. The comment casts the whole
    /// expression before it, so `s` keeps its own type in `'id:' .. s --[[@as string]]`.
    fn cast(&self, expr: &Expr) -> Option<Type> {
        let cast = self.infer.cast_after(expr.span.end)?;
        let starts = self.cast_starts.get_or_init(|| {
            let mut finder = CastTargets { infer: self.infer, starts: FxHashMap::default() };
            finder.visit_block(&self.infer.ctx.chunk.block);
            finder.starts
        });
        (starts.get(&expr.span.end) == Some(&expr.span.start)).then_some(cast)
    }

    /// The type of a local where it is read at `offset`: the declared types of the values that may
    /// reach the read, as the guards and casts around the read narrow them. A value whose type is not
    /// declared leaves the local `unknown`, unless a `---@cast` line types it. A `+T` entry adds to a
    /// type, so one that is not known stays so.
    fn local(&self, id: LocalId, offset: u32, depth: u32) -> Type {
        self.infer.narrowed_with(id, offset, &|origin| self.origin(id, origin, depth), true)
    }

    /// The declared type of the local `id` where it is read at `offset`, as `of` gives it for a read
    /// there.
    pub fn at(&self, id: LocalId, offset: u32) -> Type {
        self.local(id, offset, 0)
    }

    /// Whether the guards where the local `id` is read at `offset` rule out `value` for each of the
    /// values that may reach the read and be it.
    pub fn rules_out(&self, id: LocalId, offset: u32, value: &Type) -> bool {
        self.infer.rules_out_with(id, offset, value, &|origin| self.origin(id, origin, 0))
    }

    /// The declared type of the value that `origin` gives the local `id`, or `None` while it is
    /// being found. An assignment gives the declared type of the value, bounded by the annotation
    /// of the local, or the `---@type` above it.
    fn origin(&self, id: LocalId, origin: u32, depth: u32) -> Option<Type> {
        let local = self.infer.ctx.resolution.local(id);
        if !local.refs.iter().any(|r| r.write) {
            return Some(match depth {
                0 => self.declaration(id),
                _ => self.local_declaration(id, depth),
            });
        }
        let key = (id, origin);
        if let Some(ty) = self.origins.borrow().get(&key) {
            return Some(ty.clone());
        }
        // The values given before it first, and none given after it that are not known yet, as for
        // `Infer::origin_type`.
        if self.in_progress.borrow().iter().any(|(local, other)| *local == id && *other <= origin) {
            return None;
        }
        self.in_progress.borrow_mut().insert(key);
        for earlier in self.infer.ctx.flow().chained(id).iter().take_while(|earlier| **earlier < origin) {
            self.origin(id, *earlier, depth);
        }
        let ty = match self.infer.ctx.flow().origin(origin) {
            Origin::Declaration => self.declared_value(id, depth),
            Origin::Assignment { stmt, index } => self.assigned(id, origin, stmt, index, depth),
            Origin::Compound { stmt } => match &stmt.kind {
                StmtKind::CompoundAssign { op: BinOp::Concat, .. } => Type::String,
                _ => Type::Unknown,
            },
            Origin::Function { .. } => self.infer.origin_type(id, origin).unwrap_or_default(),
        };
        self.in_progress.borrow_mut().remove(&key);
        self.origins.borrow_mut().insert(key, ty.clone());
        Some(ty)
    }

    /// The declared type of value `index` of the assignment `stmt` to the local `id`, which gives it
    /// the value of `origin`.
    fn assigned(&self, id: LocalId, origin: u32, stmt: &Stmt, index: usize, depth: u32) -> Type {
        let StmtKind::Assign { exprs, .. } = &stmt.kind else { return Type::Unknown };
        let doc = self.infer.ctx.doc_at(stmt.span.start);
        if let Some(ty) = doc.type_at(index).filter(|_| doc.declared_class().is_none()) {
            return ty.clone();
        }
        let single = |expr: &Expr| self.declared(expr, depth + 1);
        // `name = tonumber(name) --[[@as number]]` casts the first value of the call.
        let multi = |call: &Expr| {
            let mut values = self.returned(call).unwrap_or_default();
            if let Some(cast) = self.cast(call) {
                match values.first_mut() {
                    Some(first) => *first = cast,
                    None => values.push(cast),
                }
            }
            values
        };
        let value = assigned_value(exprs, index, single, multi);
        match self.infer.annotation(id) {
            // A value with no declared type, as `name or 'none'`, still tells which parts of the
            // annotation it may be.
            Some(_) if value.is_unknown() => self.infer.origin_type(id, origin).unwrap_or_default(),
            Some(declared) => self.infer.bounded(&declared, &value),
            None => value,
        }
    }

    /// The type a local is declared with, without what the guards and casts after its declaration
    /// tell. One that is assigned again after its declaration is `unknown`, since its declared type
    /// may not be what it holds.
    pub fn declaration(&self, id: LocalId) -> Type {
        if let Some(ty) = self.declarations.borrow().get(&id) {
            return ty.clone();
        }
        let ty = self.local_declaration(id, 0);
        self.declarations.borrow_mut().insert(id, ty.clone());
        ty
    }

    fn local_declaration(&self, id: LocalId, depth: u32) -> Type {
        let local = self.infer.ctx.resolution.local(id);
        if local.refs.iter().any(|r| r.write) && !(self.keeps_annotations && self.is_annotated(local)) {
            return Type::Unknown;
        }
        self.declared_value(id, depth)
    }

    /// The type that the declaration of the local `id` gives it, whatever is assigned to it later.
    fn declared_value(&self, id: LocalId, depth: u32) -> Type {
        let local = self.infer.ctx.resolution.local(id);
        match self.infer.ctx.decl(local.decl.start) {
            Some(Decl::Local { stmt, index }) => self.local_value(id, stmt, *index, depth),
            Some(Decl::Param { .. }) if has_param_line(self.infer, local) => self.infer.local_type(id),
            Some(Decl::Param { expected: Some(Expected::Arg { .. }), .. }) => self.callback_param(id, depth),
            Some(Decl::LocalFunction { .. } | Decl::SelfParam { .. }) => self.infer.local_type(id),
            Some(Decl::NumericFor) => Type::Number,
            _ => Type::Unknown,
        }
    }

    /// What the callee of a call declares for the parameter `id` of a function passed to it, with the
    /// generics bound from what the other arguments of the call declare: `value` of
    /// `onValue('Player', function(value) end)` is a `Player` for `fun(value: T)` and a `` `T` ``.
    fn callback_param(&self, id: LocalId, depth: u32) -> Type {
        let declared = |arg: &Expr| self.declared(arg, depth + 1);
        self.infer.declared_callback_param(id, declared).unwrap_or_default()
    }

    /// Whether a `---@type` above its declaration or a `@param` line types the local.
    fn is_annotated(&self, local: &Local) -> bool {
        match self.infer.ctx.decl(local.decl.start) {
            Some(Decl::Local { stmt, index }) => self.infer.ctx.doc_at(stmt.span.start).type_at(*index).is_some(),
            Some(Decl::Param { .. }) => has_param_line(self.infer, local),
            _ => false,
        }
    }

    /// What the `local` statement `stmt` gives the name at `index`: its `---@type`, or the declared
    /// type of the value it takes. A literal written out is widened, as `local mode = 'dev'` is a
    /// setting to change.
    fn local_value(&self, id: LocalId, stmt: &Stmt, index: usize, depth: u32) -> Type {
        let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return Type::Unknown };
        let doc = self.infer.ctx.doc_at(stmt.span.start);
        if doc.type_at(index).is_some() || !doc.classes.is_empty() {
            return self.infer.local_type(id);
        }
        match exprs.get(index) {
            Some(value) => {
                let ty = self.declared(value, depth + 1);
                let is_literal = matches!(
                    value.unparen().kind,
                    ExprKind::True | ExprKind::False | ExprKind::Number(_) | ExprKind::String(_)
                );
                if is_literal {
                    ty.widen()
                } else {
                    ty
                }
            }
            // A value of the call that ends the list.
            None => {
                let returned = exprs.last().and_then(|last| self.returned(last));
                returned.and_then(|values| values.into_iter().nth(index + 1 - exprs.len())).unwrap_or_default()
            }
        }
    }

    /// The type of a global typed as one class by everything that sets it. Its other values are
    /// inferred from what is assigned to it.
    fn global(&self, name: &str) -> Type {
        let symbols = self.infer.index.globals_named(name, self.classes.file());
        match symbols.split_first() {
            Some(((_, first), rest))
                if matches!(first.ty, Type::Named(..)) && rest.iter().all(|(_, symbol)| symbol.ty == first.ty) =>
            {
                first.ty.clone()
            }
            _ => Type::Unknown,
        }
    }

    /// The type that an assignment to `target` has to store: what its local is declared with,
    /// whatever it holds before, or the declared type of the field it sets, which no guard narrows.
    pub fn target(&self, target: &Expr) -> Type {
        match &target.kind {
            ExprKind::Name(name) => match self.infer.ctx.resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => self.declaration(id),
                _ => self.of(target),
            },
            ExprKind::Field { base, name, .. } => self.field(base, &name.text, 0),
            ExprKind::Index { base, index, .. } => match index.as_string() {
                Some(name) => self.field(base, name, 0),
                None => Type::Unknown,
            },
            _ => self.of(target),
        }
    }

    /// The type that the `@field`s of a class give `name` of `base`. Fields of other tables are
    /// inferred from what is assigned to them.
    fn field(&self, base: &Expr, name: &str, depth: u32) -> Type {
        let base = self.declared(base, depth + 1);
        let from = self.classes.file();
        let Some((class, args, from)) = self.classes.class_of(&base, from) else { return Type::Unknown };
        self.classes.field_type(&class, &args, from, &Key::Name(name)).map(|(ty, _)| ty).unwrap_or_default()
    }

    /// What `call` returns when its function declares it. A native documented as `boolean` can
    /// give scripts `1` instead of `true`, so code compares its result with either.
    fn returned(&self, call: &Expr) -> Option<Vec<Type>> {
        let values = self.infer.declared_returns(call).or_else(|| self.callback_returns(call))?;
        if !self.is_native_call(call) {
            return Some(values);
        }
        let as_returned = |ty: Type| match ty {
            Type::Boolean => Type::union([Type::Boolean, Type::Integer]),
            other => other,
        };
        Some(values.into_iter().map(as_returned).collect())
    }

    /// What a call of a parameter returns when the callee of its function declares the function it
    /// holds: `get()` gives a `boolean` for `get` of `fun(get: fun(): T)` with `T` bound to one. The
    /// generics that the other arguments of that call declare bind values that are declared too.
    fn callback_returns(&self, call: &Expr) -> Option<Vec<Type>> {
        let ExprKind::Call { callee, args, .. } = &call.kind else { return None };
        let ExprKind::Name(name) = &callee.unparen().kind else { return None };
        let Some(Resolved::Local(id)) = self.infer.ctx.resolution.resolve_at(name.span.start) else { return None };
        if !matches!(self.infer.ctx.decl(self.infer.ctx.resolution.local(id).decl.start), Some(Decl::Param { .. })) {
            return None;
        }
        let fun = self.infer.fun_of(&self.declaration(id))?;
        let fun = self.infer.call_signature(&fun, args, false, call.span.start);
        fun.generics.is_empty().then(|| fun.returns.clone())
    }

    fn is_native_call(&self, call: &Expr) -> bool {
        matches!(&call.kind, ExprKind::Call { callee, .. } if self.is_native(callee, 0))
    }

    /// Whether `callee` names a native, or a local that holds one as `local IsCamActive = IsCamActive`
    /// does.
    pub fn is_native(&self, callee: &Expr, depth: u32) -> bool {
        let ExprKind::Name(name) = &callee.unparen().kind else { return false };
        let ctx = self.infer.ctx;
        let Some(Resolved::Local(id)) = ctx.resolution.resolve_at(name.span.start) else {
            return self.infer.index.globals_named(&name.text, self.classes.file()).is_empty()
                && native(&name.text).is_some();
        };
        let Some(Decl::Local { stmt, index }) = ctx.decl(ctx.resolution.local(id).decl.start) else { return false };
        let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return false };
        depth < MAX_DEPTH && exprs.get(*index).is_some_and(|value| self.is_native(value, depth + 1))
    }
}

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_expr(&mut self, expr: &'c Expr) {
        if let ExprKind::Binary { op: op @ (BinOp::Eq | BinOp::Ne), lhs, rhs, .. } = &expr.kind {
            let (left, right) = (self.declared.of(lhs), self.declared.of(rhs));
            if self.declared.classes.never_equal(&left, &right) {
                let answer = if *op == BinOp::Eq { "false" } else { "true" };
                self.out.push((expr.span, format!("Comparing `{left}` with `{right}` is always {answer}")));
            }
        }
        visit::walk_expr(self, expr);
    }
}

/// Finds the expressions that `--[[@as T]]` comments cast: of those that end where one starts,
/// the outermost.
struct CastTargets<'a, 'b> {
    infer: &'a Infer<'b>,
    starts: FxHashMap<u32, u32>,
}

impl<'c> Visitor<'c> for CastTargets<'_, '_> {
    fn visit_expr(&mut self, expr: &'c Expr) {
        if self.infer.cast_after(expr.span.end).is_some() {
            let start = self.starts.entry(expr.span.end).or_insert(expr.span.start);
            *start = (*start).min(expr.span.start);
        }
        visit::walk_expr(self, expr);
    }
}
