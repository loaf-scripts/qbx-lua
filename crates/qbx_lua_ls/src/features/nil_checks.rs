//! `need-check-nil`: a local that may hold `nil` or `false` where code reads a field of it, calls
//! it, uses it as a key it stores a value under, does arithmetic, concatenation, `#` or a `<`, `<=`,
//! `>` or `>=` comparison with it, or gives it as a bound of a numeric `for`, all of which raise an
//! error for such a value. `name:upper()` after `local name = GetName()`, for a function declared
//! to return `string?`, is one. A local that may hold `nil` is also reported where a call passes it
//! for a parameter that does not take `nil` in any signature the call may use, as
//! `param-type-mismatch` reads them; natives are left out, as their arguments are. lua-language-server
//! reports those arguments as `param-type-mismatch`, so suppressing that rule silences them too.
//!
//! As for `impossible-comparison`, only declared types count, whatever declares them: an
//! annotation of the local, or the `@return` or `@field` of a function, class or stub. The type is
//! the one the guards and casts around the read leave. Guards that leave no value of it keep it whole,
//! but a `nil` or `false` that they rule out stays out: `round` is no `nil` in
//! `round and (round == true or i < round)`, whatever else it holds. The rest is left alone:
//!
//! - Fields and the values of calls, which guards do not narrow.
//! - Locals that are assigned again, unless a `---@cast` types them: a guard tells nothing about
//!   the value an assignment replaces, and `name = name or 'none'` is how code fills in a `nil`.
//! - A value that a guard on another value of the same call covers, as `err` of
//!   `local fn, err = load(code)` after `if not fn then`: annotations declare such values one by one.
//! - What a call gives for `source` alone, as `ESX.GetPlayerFromId(source)`: lookups of the player
//!   whose event or callback runs only miss one that has just left.
//! - Reads after one that always runs before them, which raises the error first, and reads after
//!   one reported in the same function, which points at the same missing check.

use qbx_lua_analysis::scope::{FuncId, LocalId, LocalKind, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;
use rustc_hash::FxHashSet;

use super::arguments::{candidates, expected, mismatch, param_for, signatures, takes_nil};
use super::class_tables::Classes;
use super::comparisons::Declared;
use crate::infer::{Decl, Infer};
use crate::types::{FunType, Param, Type};

/// What `need-check-nil` finds in a file, each with the message naming the type.
pub struct UncheckedNils {
    /// Each read of a local that may hold `nil` or `false` there.
    pub reads: Vec<(Span, String)>,
    /// Each local that may hold `nil` where a call passes it for a parameter that does not take it.
    pub arguments: Vec<(Span, String)>,
}

/// The reads and arguments of locals that may hold a missing value there.
pub fn unchecked_nils(infer: &Infer, chunk: &Chunk) -> UncheckedNils {
    let mut finder = Finder {
        infer,
        declared: Declared::new(infer),
        checked: Vec::new(),
        sometimes: 0,
        until: chunk.block.span.end,
        reported: FxHashSet::default(),
        out: Vec::new(),
        passed: Vec::new(),
    };
    finder.visit_block(&chunk.block);
    UncheckedNils { reads: finder.out, arguments: finder.passed }
}

struct Finder<'a, 'b> {
    infer: &'a Infer<'b>,
    declared: Declared<'a, 'b>,
    /// The locals that a reported read showed to hold a value, each with the code that runs after it.
    checked: Vec<(LocalId, Span)>,
    /// How many of the parts around the expression being visited only run sometimes: the right
    /// sides of `and` and `or`, the conditions of `elseif`, and that of a `repeat` whose body can
    /// `break` out of it.
    sometimes: u32,
    /// Where the code that a read in the statement being visited runs before ends: at the end of
    /// its block, or at a label a `goto` can reach without the read.
    until: u32,
    /// The locals reported so far, each with the function the read is in.
    reported: FxHashSet<(LocalId, FuncId)>,
    out: Vec<(Span, String)>,
    /// The arguments reported, apart from the reads.
    passed: Vec<(Span, String)>,
}

impl Finder<'_, '_> {
    /// Reports `operand` when it names a local that may hold `nil` or `false` there.
    fn check(&mut self, operand: &Expr) {
        self.check_value(operand, None);
    }

    /// Reports `operand` when it names a local that may hold `nil` or `false` there, or only `nil`
    /// when a call passes it for `param`, which does not take it.
    fn check_value(&mut self, operand: &Expr, param: Option<&Param>) {
        let ExprKind::Name(name) = &operand.unparen().kind else { return };
        let ctx = self.infer.ctx;
        let Some(Resolved::Local(id)) = ctx.resolution.resolve_at(name.span.start) else { return };
        let local = ctx.resolution.local(id);
        let at = name.span.start;
        // `self` is the value a method is called on.
        if local.kind == LocalKind::ImplicitSelf {
            return;
        }
        if self.checked.iter().any(|(checked, span)| *checked == id && span.contains(at)) {
            return;
        }
        let ty = self.declared.of(operand.unparen());
        let reaches = |value: &Type| !self.infer.rules_out(id, at, value);
        let missing = missing_value(&self.infer.expand_aliases(&ty, 0), reaches);
        let Some(missing) = missing.filter(|missing| param.is_none() || *missing == "nil") else { return };
        // `local vehicle, coords = lib.getClosestVehicle(...)` declares both as optional, and the
        // guard on `vehicle` is the check for `coords` as well.
        let guards = ctx.guards();
        let linked = guards.linked(id).unwrap_or_default();
        if linked.iter().any(|(other, _)| *other != id && guards.at(*other, at).next().is_some()) {
            return;
        }
        let assigned = local.refs.iter().any(|r| r.write);
        if !assigned && self.is_lookup_of_source(id) {
            return;
        }
        let func = local.refs.iter().find(|r| r.span.start == at).map_or(local.func, |r| r.func);
        if !self.reported.insert((id, func)) {
            return;
        }
        // A call passes the missing value on, which raises no error there.
        if self.sometimes == 0 && !assigned && param.is_none() {
            self.checked.push((id, Span::new(name.span.end, self.until.max(name.span.end))));
        }
        match param {
            Some(param) => self.passed.push((
                name.span,
                format!(
                    "`{}` may be nil, which parameter `{}` of type `{}` does not take: its type here is `{ty}`",
                    name.text,
                    param.name,
                    expected(param)
                ),
            )),
            None => self.out.push((name.span, format!("`{}` may be {missing}: its type here is `{ty}`", name.text))),
        }
    }

    /// Reports the locals among `args` that may hold `nil` where each signature the call may use
    /// passes them for a parameter that does not take `nil`. An argument of another type than the
    /// parameter takes is left to `param-type-mismatch`.
    fn check_arguments(&mut self, call: &Expr, base: &Expr, method: Option<&Name>, args: &[Expr]) {
        let infer = self.infer;
        let via_colon = method.is_some();
        let classes = Classes::new(infer);
        let mut resolved = None;
        for (index, arg) in args.iter().enumerate() {
            if !self.may_be_nil(arg) {
                continue;
            }
            let found = resolved.get_or_insert_with(|| signatures(infer, &self.declared, call, base, method, args));
            let found: Vec<&FunType> = found.iter().map(AsRef::as_ref).collect();
            let given = self.declared.of(arg);
            let params: Option<Vec<&Param>> = candidates(infer, &found, via_colon, args)
                .into_iter()
                .map(|fun| {
                    let param = param_for(fun, via_colon, index)?;
                    let refuses = !takes_nil(&classes, param) && mismatch(&classes, fun, param, &given).is_none();
                    refuses.then_some(param)
                })
                .collect();
            if let Some(param) = params.as_ref().and_then(|params| params.first()) {
                self.check_value(arg, Some(param));
            }
        }
    }

    /// Whether `arg` names a local whose declared type allows `nil` there.
    fn may_be_nil(&self, arg: &Expr) -> bool {
        let ExprKind::Name(name) = &arg.unparen().kind else { return false };
        matches!(self.infer.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Local(_)))
            && missing_value(&self.infer.expand_aliases(&self.declared.of(arg), 0), |_| true) == Some("nil")
    }

    fn sometimes(&mut self, visit: impl FnOnce(&mut Self)) {
        self.sometimes += 1;
        visit(self);
        self.sometimes -= 1;
    }

    /// Whether `id` holds what a call gives for `source` alone, as after
    /// `local player = exports.qbx_core:GetPlayer(src)` with `local src = source`.
    fn is_lookup_of_source(&self, id: LocalId) -> bool {
        let Some(value) = self.value_of(id) else { return false };
        match &value.unparen().kind {
            ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => {
                matches!(args.as_slice(), [arg] if self.is_source(arg))
            }
            _ => false,
        }
    }

    /// Whether `expr` is `source`, or a local that is never assigned again and holds it.
    fn is_source(&self, expr: &Expr) -> bool {
        let ExprKind::Name(name) = &expr.unparen().kind else { return false };
        if name.text == "source" {
            return true;
        }
        let ctx = self.infer.ctx;
        let Some(Resolved::Local(id)) = ctx.resolution.resolve_at(name.span.start) else { return false };
        !ctx.resolution.local(id).refs.iter().any(|r| r.write) && self.value_of(id).is_some_and(|v| self.is_source(v))
    }

    /// The value that the `local` statement declaring `id` gives it: the expression in its
    /// position, or the call that ends the list.
    fn value_of(&self, id: LocalId) -> Option<&Expr> {
        let ctx = self.infer.ctx;
        let Some(Decl::Local { stmt, index }) = ctx.decl(ctx.resolution.local(id).decl.start) else { return None };
        let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return None };
        exprs.get(*index).or_else(|| exprs.last().filter(|last| last.is_call()))
    }
}

/// What `ty` allows that raises the error and `reaches` the read: `nil`, or else `false`. `None` when
/// it allows neither, or nothing else, as for a local declared as `nil`.
fn missing_value(ty: &Type, reaches: impl Fn(&Type) -> bool) -> Option<&'static str> {
    let parts = match ty {
        Type::Union(parts) => parts.as_slice(),
        one => std::slice::from_ref(one),
    };
    if parts.iter().all(|part| matches!(part, Type::Nil | Type::BooleanLit(false))) {
        return None;
    }
    let allows = |value: Type| parts.contains(&value) && reaches(&value);
    if allows(Type::Nil) {
        Some("nil")
    } else if allows(Type::BooleanLit(false)) {
        Some("false")
    } else {
        None
    }
}

/// Whether `op` raises an error for a `nil` or `false` operand. `==` and `~=` compare any values.
fn needs_value(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Add
            | BinOp::Sub
            | BinOp::Mul
            | BinOp::Div
            | BinOp::IDiv
            | BinOp::Mod
            | BinOp::Pow
            | BinOp::Concat
            | BinOp::Lt
            | BinOp::Le
            | BinOp::Gt
            | BinOp::Ge
            | BinOp::BAnd
            | BinOp::BOr
            | BinOp::BXor
            | BinOp::Shl
            | BinOp::Shr
    )
}

/// Whether `block`, the body of a loop, has a `break` that leaves that loop.
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

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_block(&mut self, block: &'c Block) {
        // A block, a function body included, runs apart from the statement around it.
        let outer = (std::mem::take(&mut self.sometimes), self.until);
        for (index, stmt) in block.stmts.iter().enumerate() {
            let label = block.stmts[index + 1..].iter().find(|next| matches!(next.kind, StmtKind::Label(_)));
            self.until = label.map_or(block.span.end, |label| label.span.start);
            self.visit_stmt(stmt);
        }
        (self.sometimes, self.until) = outer;
    }

    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        match &stmt.kind {
            // Storing a value under a `nil` key raises an error, while reading one gives `nil`.
            StmtKind::Assign { targets, .. } => {
                for target in targets {
                    if let ExprKind::Index { index, .. } = &target.kind {
                        self.check(index);
                    }
                }
            }
            StmtKind::CompoundAssign { op, expr, .. } if needs_value(*op) => self.check(expr),
            StmtKind::NumericFor { start, limit, step, .. } => {
                for bound in [Some(start), Some(limit), step.as_ref()].into_iter().flatten() {
                    self.check(bound);
                }
            }
            // Only the first condition of an `if` always runs.
            StmtKind::If { branches, else_block } => {
                for (index, branch) in branches.iter().enumerate() {
                    if index == 0 {
                        self.visit_expr(&branch.cond);
                    } else {
                        self.sometimes(|finder| finder.visit_expr(&branch.cond));
                    }
                    self.visit_block(&branch.block);
                }
                if let Some(block) = else_block {
                    self.visit_block(block);
                }
                return;
            }
            // A `break` leaves the loop without running the condition.
            StmtKind::Repeat { body, cond } => {
                self.visit_block(body);
                if breaks(body) {
                    self.sometimes(|finder| finder.visit_expr(cond));
                } else {
                    self.visit_expr(cond);
                }
                return;
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'c Expr) {
        match &expr.kind {
            // `?.`, `?[` and `?:` give `nil` for a `nil` value.
            ExprKind::Field { base, safe: false, .. }
            | ExprKind::Index { base, safe: false, .. }
            | ExprKind::MethodCall { base, safe: false, .. } => self.check(base),
            ExprKind::Call { callee, .. } => self.check(callee),
            ExprKind::Unary { op: UnOp::Len | UnOp::Neg | UnOp::BNot, expr: operand } => self.check(operand),
            ExprKind::Binary { op, lhs, rhs, .. } if needs_value(*op) => {
                self.check(lhs);
                self.check(rhs);
            }
            // The right side of `and` and `or` only runs for some values of the left one.
            ExprKind::Binary { op: BinOp::And | BinOp::Or, lhs, rhs, .. } => {
                self.visit_expr(lhs);
                self.sometimes(|finder| finder.visit_expr(rhs));
                return;
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
        // A call passes its arguments once it has read them all.
        match &expr.kind {
            ExprKind::Call { callee, args, .. } => self.check_arguments(expr, callee, None, args),
            ExprKind::MethodCall { base, method, args, .. } => self.check_arguments(expr, base, Some(method), args),
            _ => {}
        }
    }
}
