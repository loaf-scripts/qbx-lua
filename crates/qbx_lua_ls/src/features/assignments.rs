//! `assign-type-mismatch` for variables: a `local` or an assignment with a `---@type` above it has
//! to store values of that type, and so does a later assignment to a local declared with
//! `---@type` or to a parameter documented with `@param`. As for `@return`, a value of a different
//! kind, or a literal the type does not list, is a mismatch, for each type a union lists, `nil`
//! included, though not the `nil` of a field read. `class_tables` checks the fields of class
//! tables.

use qbx_lua_analysis::scope::{LocalId, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use super::class_tables::{checked_value, mismatched_fields, unknown_fields, Classes};
use super::unknown_types::{given_values, has_param_line, is_typed, value_at};
use crate::infer::{Decl, Infer};
use crate::types::Type;

/// Each value stored in a class field or a variable whose type does not take it, with the message
/// naming both types. A value that its field rejects is reported for the field alone.
pub fn mismatched_assignments(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let mut out = mismatched_fields(infer, chunk);
    let mut finder = Finder { infer, classes: Classes::new(infer), unknowns: None, out: Vec::new() };
    finder.visit_block(&chunk.block);
    finder.out.retain(|(span, _)| !out.iter().any(|(reported, _)| reported == span));
    out.append(&mut finder.out);
    out
}

/// Each value of no known type, of those `pick` picks, stored in a class field or a variable whose
/// type is declared, with the message naming the target.
pub fn unknown_assignments(infer: &Infer, chunk: &Chunk, pick: &dyn Fn(&Expr) -> bool) -> Vec<(Span, String)> {
    let mut out = unknown_fields(infer, chunk, pick);
    let mut finder = Finder { infer, classes: Classes::new(infer), unknowns: Some(pick), out: Vec::new() };
    finder.visit_block(&chunk.block);
    finder.out.retain(|(span, _)| !out.iter().any(|(reported, _)| reported == span));
    out.append(&mut finder.out);
    out
}

struct Finder<'a, 'b> {
    infer: &'a Infer<'b>,
    classes: Classes<'a, 'b>,
    /// When the values of no known type are looked for instead of mismatches, which of them count.
    unknowns: Option<&'a dyn Fn(&Expr) -> bool>,
    out: Vec<(Span, String)>,
}

impl Finder<'_, '_> {
    /// The `---@type` above `stmt` for the name at `index`. `---@class Name` above a table
    /// declares the class rather than a value of it.
    fn stmt_type(&self, stmt: &Stmt, index: usize) -> Option<Type> {
        let doc = self.infer.ctx.doc_at(stmt.span.start);
        doc.type_at(index).filter(|_| doc.declared_class().is_none()).cloned()
    }

    /// The type a local is declared with: the `---@type` above its `local` statement, or its
    /// `@param` line. What the guards around an assignment tell about the local does not limit
    /// what it may be given.
    fn local_type(&self, id: LocalId) -> Option<Type> {
        let ctx = self.infer.ctx;
        let local = ctx.resolution.local(id);
        match ctx.decl(local.decl.start)? {
            Decl::Local { stmt, index } => self.stmt_type(stmt, *index),
            Decl::Param { .. } if has_param_line(self.infer, local) => Some(self.infer.local_type(id)),
            _ => None,
        }
    }

    /// Reports each of `exprs` that the declared type of the target it is stored in does not take.
    fn check(&mut self, targets: &[Target], exprs: &[Expr]) {
        if targets.iter().all(|target| target.ty.is_none()) {
            return;
        }
        let from = self.classes.file();
        let mut values = self.classes.values(exprs);
        if self.unknowns.is_some() {
            values = given_values(self.infer, exprs, values);
        }
        for (index, (target, (given, span))) in targets.iter().zip(values).enumerate() {
            let Some(expected) = &target.ty else { continue };
            // LuaLS lets the statement whose `---@type` declares a name give it `nil`.
            if target.annotated && exprs.get(index).is_some_and(|expr| matches!(expr.kind, ExprKind::Nil)) {
                continue;
            }
            if let Some(pick) = self.unknowns {
                if given.is_unknown() && is_typed(expected) && value_at(exprs, index).is_some_and(pick) {
                    let name = target.name.text(self.infer.ctx.source);
                    self.out.push((
                        span,
                        format!("The type of the value assigned to `{name}` of type `{expected}` is unknown"),
                    ));
                }
                continue;
            }
            let given = checked_value(self.infer, value_at(exprs, index), given);
            if let Some(part) = self.classes.rejected_part(expected, from, &given) {
                let shown = self.classes.shown(expected, from, &given, &part);
                let name = target.name.text(self.infer.ctx.source);
                self.out.push((span, format!("Cannot assign `{shown}` to `{name}` of type `{expected}`")));
            }
        }
    }
}

/// Where a value is stored: the name it is written to, the type declared for it, and whether the
/// `---@type` of the statement that stores it declares that type.
struct Target {
    name: Span,
    ty: Option<Type>,
    annotated: bool,
}

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        match &stmt.kind {
            StmtKind::Local { names, exprs, in_unpack: false } => {
                let declared = |(index, name): (usize, &AttribName)| {
                    let ty = self.stmt_type(stmt, index);
                    Target { name: name.name.span, annotated: ty.is_some(), ty }
                };
                let targets: Vec<_> = names.iter().enumerate().map(declared).collect();
                self.check(&targets, exprs);
            }
            StmtKind::Assign { targets, exprs } => {
                // The `---@type` above the assignment, or else the type its target is declared with.
                let declared = |(index, target): (usize, &Expr)| {
                    let own = self.stmt_type(stmt, index);
                    let annotated = own.is_some();
                    let ty = own.or_else(|| match &target.kind {
                        ExprKind::Name(name) => match self.infer.ctx.resolution.resolve_at(name.span.start) {
                            Some(Resolved::Local(id)) => self.local_type(id),
                            _ => None,
                        },
                        _ => None,
                    });
                    Target { name: target.span, ty, annotated }
                };
                let targets: Vec<_> = targets.iter().enumerate().map(declared).collect();
                self.check(&targets, exprs);
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }
}
