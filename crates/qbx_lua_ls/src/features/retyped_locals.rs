//! `cast-local-type`: an assignment that gives a local a value its type does not take, where nothing
//! annotates that type, as `speed = "5"` after `local speed = 5`. As in lua-language-server and
//! TypeScript, such a local has the type of the value it is declared with. A literal written out
//! stands for its kind, so `local mode = 'dev'` takes any string and `local count = 0` any number,
//! while the literals that a function declares it returns, as the `'a'|'b'` of `getMode()`, are the
//! only ones it takes. A loop variable has the type its loop gives it. A local declared without a
//! value, or as `nil`, `unknown` or `any`, takes any value.
//!
//! The check is that of `assign-type-mismatch`, for each type the value may be: a different kind of
//! value, or a literal the type does not list, so `nil`, or a `string?`, needs a local that may be
//! `nil`. A native returns a `BOOL` as a boolean or an integer, so either passes for the other where
//! a native gives it. Locals typed with `---@type` or `@param` are left to `assign-type-mismatch`,
//! `<const>` and `<close>` ones to `const-reassign`.
//!
//! As in lua-language-server, a local declared with a table constructor may be cleared with `nil`,
//! and the `nil` and `false` of a value read from a field or a key are left out. So is a `nil` that
//! no annotation or stub declares: lua-language-server leaves out the one a function without
//! `@return` gives, as `nextFreePoint()` does by running past its end when it finds none, also where
//! a local passes it on. Parameters are not checked. With `strict`, as in TypeScript, all of
//! those count, and a parameter has the type of the function type its function is passed or
//! written as.

use qbx_lua_analysis::scope::{LocalId, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;
use rustc_hash::FxHashMap;

use super::arguments::reads_field;
use super::class_tables::Classes;
use super::comparisons::Declared;
use super::unknown_types::{has_param_line, value_at};
use crate::infer::{Decl, Expected, Infer};
use crate::narrow::DECLARATION;
use crate::types::Type;

/// Each value assigned to a local that the type it is declared with does not take, with the message
/// naming both types.
pub fn retyped_locals(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let mut finder = Finder {
        infer,
        classes: Classes::new(infer),
        declared: Declared::new(infer),
        types: FxHashMap::default(),
        out: Vec::new(),
    };
    finder.visit_block(&chunk.block);
    finder.out
}

struct Finder<'a, 'b> {
    infer: &'a Infer<'b>,
    classes: Classes<'a, 'b>,
    declared: Declared<'a, 'b>,
    /// The type of each local assigned so far, and whether a native gives it, or `None` for one
    /// that takes any value or whose annotation `assign-type-mismatch` checks.
    types: FxHashMap<LocalId, Option<(Type, bool)>>,
    out: Vec<(Span, String)>,
}

impl Finder<'_, '_> {
    /// The type of the values the local `id` takes, and whether a native gives it, or `None` when
    /// it takes any value or an annotation declares its type.
    fn local_type(&mut self, id: LocalId) -> Option<(Type, bool)> {
        if let Some(ty) = self.types.get(&id) {
            return ty.clone();
        }
        let ty = self
            .declaration(id)
            .map(|(ty, native)| (single(ty), native))
            .filter(|(ty, _)| !matches!(ty, Type::Unknown | Type::Any | Type::Nil));
        self.types.insert(id, ty.clone());
        ty
    }

    /// The type that the declaration of the local `id` gives it, unless an annotation declares it,
    /// and whether a native gives it.
    fn declaration(&self, id: LocalId) -> Option<(Type, bool)> {
        let ctx = self.infer.ctx;
        let local = ctx.resolution.local(id);
        // `_` is a name for values that are thrown away.
        if local.name == "_" || local.attrib.is_some() {
            return None;
        }
        let ty = match ctx.decl(local.decl.start)? {
            Decl::Local { stmt, index } => return self.local_value(id, stmt, *index),
            Decl::LocalFunction { .. } | Decl::NumericFor | Decl::GenericFor { .. } => {
                self.infer.origin_type(id, DECLARATION)?.widen()
            }
            Decl::Param { .. } | Decl::SelfParam { .. } if !self.infer.strict() => return None,
            // What the callee declares, rather than what the calls that reach the function pass.
            Decl::Param { expected: Some(Expected::Arg { .. }), .. } => {
                self.infer.declared_callback_param(id, |arg| self.declared.of(arg))?
            }
            Decl::Param { expected: Some(_), .. } if !has_param_line(self.infer, local) => self.infer.local_type(id),
            Decl::Param { .. } | Decl::SelfParam { .. } => return None,
        };
        Some((ty, false))
    }

    /// The type of the value that the `local` statement `stmt` gives the name at `index`, with the
    /// literals written out widened to their kind, and whether a native gives it.
    fn local_value(&self, id: LocalId, stmt: &Stmt, index: usize) -> Option<(Type, bool)> {
        let StmtKind::Local { exprs, .. } = &stmt.kind else { return None };
        if self.infer.ctx.doc_at(stmt.span.start).type_at(index).is_some() {
            return None;
        }
        let native = self.is_native_value(exprs, index);
        let ty = self.infer.origin_type(id, DECLARATION)?;
        let widened = ty.widen();
        if widened == ty {
            return Some((ty, native));
        }
        // A literal that a declaration lists, rather than one that is written out, is a value the
        // local is meant to hold.
        let declared = match exprs.get(index) {
            Some(value) if !is_literal(value) => self.declared.of(value),
            Some(_) => Type::Unknown,
            None => exprs
                .last()
                .filter(|last| last.is_multi_value())
                .and_then(|last| self.infer.declared_returns(last))
                .and_then(|values| values.into_iter().nth(index + 1 - exprs.len()))
                .unwrap_or_default(),
        };
        Some((if declared.widen() == declared { widened } else { ty }, native))
    }

    /// Whether a native call gives the value at `index` of those that `exprs` assign.
    fn is_native_value(&self, exprs: &[Expr], index: usize) -> bool {
        let value = exprs.get(index.min(exprs.len().saturating_sub(1)));
        value.is_some_and(|value| match &value.unparen().kind {
            ExprKind::Call { callee, .. } => self.declared.is_native(callee, 0),
            _ => false,
        })
    }

    /// Reports `given`, written at `span`, when the local that `target` names does not take it.
    /// `native` tells whether a native gives it.
    fn check(&mut self, target: &Expr, given: Type, native: bool, span: Span) {
        let ExprKind::Name(name) = &target.kind else { return };
        let Some(Resolved::Local(id)) = self.infer.ctx.resolution.resolve_at(name.span.start) else { return };
        let Some((expected, declared_by_native)) = self.local_type(id) else { return };
        let given = single(given);
        let from = self.classes.file();
        let takes = |ty: &Type| !self.classes.rejects(&expected, from, ty);
        let takes_part = |part: &Type| match part {
            Type::Unknown | Type::Any => true,
            // A native returns a `BOOL` as a boolean or an integer.
            Type::Boolean | Type::BooleanLit(_) if native => takes(part) || takes(&Type::Integer),
            Type::Number | Type::Integer | Type::IntLit(_) | Type::Handle(_) if declared_by_native => {
                takes(part) || takes(&Type::Boolean)
            }
            part => takes(part),
        };
        // A table that a local is declared with can be dropped, as lua-language-server lets it.
        let clears_table = |part: &Type| *part == Type::Nil && !self.infer.strict() && self.declared_with_table(id);
        let mut parts = Vec::new();
        self.classes.flatten(&given, from, &mut parts, 0);
        if parts.iter().all(|part| takes_part(part) || clears_table(part)) {
            return;
        }
        let shown = if self.classes.literal_mismatch(&expected, from, &given) { given } else { given.widen() };
        self.out.push((span, format!("Cannot assign `{shown}` to `{}`, defined as `{expected}`", name.text)));
    }

    /// Whether a table constructor gives the local `id` the value it is declared with.
    fn declared_with_table(&self, id: LocalId) -> bool {
        let ctx = self.infer.ctx;
        let Some(Decl::Local { stmt, index }) = ctx.decl(ctx.resolution.local(id).decl.start) else { return false };
        let StmtKind::Local { exprs, .. } = &stmt.kind else { return false };
        exprs.get(*index).is_some_and(|value| matches!(value.unparen().kind, ExprKind::Table(_)))
    }

    /// The type of `given`, value `index` of those that `exprs` store, as the check reads it.
    /// Without `strict`, the `nil` and `false` of a value read from a field or a key are left out,
    /// as lua-language-server does not narrow them, and so is a `nil` that no annotation or stub
    /// declares: one that a function without `@return` gives, or a local that holds such a value.
    fn checked_value(&self, exprs: &[Expr], index: usize, given: Type) -> Type {
        let Some(value) = value_at(exprs, index).filter(|_| !self.infer.strict()) else { return given };
        if reads_field(value) {
            return self.infer.without_falsy(&given);
        }
        let from = self.classes.file();
        let holds_nil = |ty: &Type| {
            let mut parts = Vec::new();
            self.classes.flatten(ty, from, &mut parts, 0);
            parts.contains(&Type::Nil)
        };
        if !holds_nil(&given) {
            return given;
        }
        let declared = match exprs.get(index) {
            Some(value) => self.declared.of(value),
            None => self
                .infer
                .declared_returns(value)
                .and_then(|values| values.into_iter().nth(index + 1 - exprs.len()))
                .unwrap_or_default(),
        };
        match holds_nil(&declared) {
            true => given,
            false => given.without_nil(),
        }
    }

    /// What `target ..= value`, or another compound assignment, stores in the local `target` names.
    fn compound_value(&self, target: &Expr) -> Option<Type> {
        let ExprKind::Name(name) = &target.kind else { return None };
        let Some(Resolved::Local(id)) = self.infer.ctx.resolution.resolve_at(name.span.start) else { return None };
        let origin = self.infer.ctx.flow().written_at(name.span.start)?;
        self.infer.origin_type(id, origin)
    }
}

/// Whether `expr` is a literal written out.
fn is_literal(expr: &Expr) -> bool {
    matches!(expr.unparen().kind, ExprKind::True | ExprKind::False | ExprKind::Number(_) | ExprKind::String(_))
}

/// One of the values that `ty` stands for: a `string` for the `...string` a function returns.
fn single(ty: Type) -> Type {
    match ty {
        Type::Variadic(inner) => *inner,
        ty => ty,
    }
}

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        match &stmt.kind {
            StmtKind::Assign { targets, exprs } => {
                let doc = self.infer.ctx.doc_at(stmt.span.start);
                let values = self.classes.values(exprs);
                for (index, (target, (given, span))) in targets.iter().zip(values).enumerate() {
                    // A `---@type` above the assignment declares what it stores, as lua-language-server
                    // reads it, and `assign-type-mismatch` checks the value against it.
                    match doc.type_at(index).filter(|_| doc.declared_class().is_none()) {
                        Some(annotated) => self.check(target, annotated.clone(), false, span),
                        None => {
                            let native = self.is_native_value(exprs, index);
                            let given = self.checked_value(exprs, index, given);
                            self.check(target, given, native, span);
                        }
                    }
                }
            }
            StmtKind::CompoundAssign { target, .. } => {
                if let Some(given) = self.compound_value(target) {
                    self.check(target, given, false, stmt.span);
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }
}
