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
//! annotation of the local, the `@return` or `@field` of a function, class or stub, or the value
//! type of a map or indexed table, though `t[i]` in `for i = 1, #t do` is an item `t` holds. A
//! function without `@return` declares the `nil` or `false` that a `return` of it writes out or
//! leaves out, as lua-language-server infers it, but not that of running past its end. The type is
//! that of the values that may reach the read, as the guards and casts around it leave them, also
//! for a local that is assigned again. Where guards leave no value of it, the read has the type the
//! code they guard takes it to be, as lua-language-server reads it: `nil` inside `if not count then`
//! for a `number`, which holds no value that may be missing, or the whole type after a comparison
//! with a literal, but a `nil` or `false` that they rule out stays out: `round` is no `nil` in
//! `round and (round == true or i < round)`, whatever else it holds.
//!
//! Like TypeScript for a value that may be `undefined`, every read is reported, also after one that
//! would raise the error first, and only a guard on the local itself checks it: one on another value
//! of the same call tells nothing about it, unless the function declares the sets of values it
//! returns, as `@return nil | (integer, vector3)` does. The rest is left alone:
//!
//! - Fields, as in lua-language-server, and the values of calls. A local that takes the value of a
//!   field has the type that the guards on the field leave.
//! - A value whose type is not declared, as `name = name or 'none'` gives, and the missing value of
//!   `local name` before something gives it one.

use qbx_lua_analysis::scope::{LocalKind, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use super::arguments::{candidates, expected, mismatch, param_for, signatures, takes_nil};
use super::class_tables::Classes;
use super::comparisons::Declared;
use crate::infer::Infer;
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
    let declared = Declared::with_returned_nils(infer);
    let mut finder = Finder { infer, declared, out: Vec::new(), passed: Vec::new() };
    finder.visit_block(&chunk.block);
    UncheckedNils { reads: finder.out, arguments: finder.passed }
}

struct Finder<'a, 'b> {
    infer: &'a Infer<'b>,
    declared: Declared<'a, 'b>,
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
        // `self` is the value a method is called on.
        if ctx.resolution.local(id).kind == LocalKind::ImplicitSelf {
            return;
        }
        let at = name.span.start;
        let ty = self.declared.of(operand.unparen());
        let reaches = |value: &Type| !self.declared.rules_out(id, at, value);
        let missing = missing_value(&self.infer.expand_aliases(&ty, 0), reaches);
        let Some(missing) = missing.filter(|missing| param.is_none() || *missing == "nil") else { return };
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

impl<'c> Visitor<'c> for Finder<'_, '_> {
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
