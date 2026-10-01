//! The type that a value has to be where it is written, which completion offers the values of:
//! the `---@type` above the `local` or assignment that stores it, the declared type of the local
//! or class field it is assigned to, the `@field` it sets in a class-typed table, the `@return` of
//! the function that returns it, and the declared type of what `==` or `~=` compares it with.

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;
use rustc_hash::FxHashMap;

use super::class_tables::{class_tables, entries, Classes, Key};
use super::comparisons::Declared;
use crate::infer::Infer;
use crate::types::Type;

/// Where a value is written.
#[derive(Clone, Copy)]
pub enum Place {
    /// Right after the `=`, `==`, `~=` or `return` that ends at this offset, where the value is
    /// still to be written or only starts.
    After(u32),
    /// The string literal with this span.
    String(Span),
}

/// What a value has to be where it is written.
pub struct ExpectedValue {
    pub ty: Type,
    /// The value is compared with one of this type rather than stored as one, so any value the
    /// type lists can be written, but not a new value of the type such as a function.
    pub compared: bool,
}

impl ExpectedValue {
    fn stored(ty: Type) -> Self {
        Self { ty, compared: false }
    }
}

/// The type that the value at `place` has to be, when something declares it.
pub fn expected_type(infer: &Infer, chunk: &Chunk, place: Place) -> Option<ExpectedValue> {
    let mut finder = Finder {
        infer,
        declared: Declared::keeping_annotations(infer),
        place,
        function_returns: Vec::new(),
        documented: FxHashMap::default(),
        found: None,
    };
    finder.visit_block(&chunk.block);
    finder.found
}

struct Finder<'a, 'b> {
    infer: &'a Infer<'b>,
    declared: Declared<'a, 'b>,
    place: Place,
    /// The `@return` types of the functions around the statement being visited, innermost last.
    function_returns: Vec<Vec<Type>>,
    /// The `@return` types of the functions whose doc comment was visited, by the start of their
    /// parameter list.
    documented: FxHashMap<u32, Vec<Type>>,
    found: Option<ExpectedValue>,
}

impl Finder<'_, '_> {
    /// An offset that everything around the value contains.
    fn at(&self) -> u32 {
        match self.place {
            Place::After(at) => at,
            Place::String(span) => span.start,
        }
    }

    /// Whether `expr` is the value at the place. What follows an `=` that has no value yet is
    /// parsed as its value, also from the next line.
    fn is_value(&self, expr: &Expr) -> bool {
        match self.place {
            Place::After(at) => {
                let between = self.infer.ctx.source.get(at as usize..expr.span.start as usize);
                between.is_some_and(|between| between.trim().is_empty())
            }
            Place::String(span) => expr.span == span,
        }
    }

    /// The type of the `@field` that `key` sets in `table`, when the table is typed as a class.
    fn field(&self, table: &Expr, key: &Key) -> Option<Type> {
        let tables = class_tables(self.infer, self.infer.ctx.chunk);
        let found = tables.into_iter().find(|found| found.table.span == table.span)?;
        Classes::new(self.infer).field_type(&found.class, found.from, key).map(|(ty, _)| ty)
    }
}

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        if self.found.is_some() || !stmt.span.contains_inclusive(self.at()) {
            return;
        }
        for (doc, functions) in self.infer.ctx.function_docs(stmt) {
            for func in functions {
                let returns = doc.returns.iter().map(|r| self.infer.doc_type_for(stmt, func, &r.ty)).collect();
                self.documented.insert(func.params_span.start, returns);
            }
        }
        match &stmt.kind {
            StmtKind::Local { exprs, .. } => {
                if let Some(index) = exprs.iter().position(|expr| self.is_value(expr)) {
                    let doc = self.infer.ctx.doc_at(stmt.span.start);
                    // `---@class Name` above a table declares the class rather than an instance of it.
                    if doc.classes.is_empty() {
                        self.found = doc.type_at(index).cloned().map(ExpectedValue::stored);
                    }
                }
            }
            StmtKind::Assign { targets, exprs } => {
                if let Some(index) = exprs.iter().position(|expr| self.is_value(expr)) {
                    let doc = self.infer.ctx.doc_at(stmt.span.start);
                    let declared = doc.type_at(index).cloned();
                    let ty = declared.or_else(|| targets.get(index).map(|target| self.declared.of(target)));
                    self.found = ty.map(ExpectedValue::stored);
                }
            }
            StmtKind::Return(exprs) => {
                let index = match self.place {
                    Place::After(at) => (stmt.span.start + "return".len() as u32 == at).then_some(0),
                    Place::String(_) => exprs.iter().position(|expr| self.is_value(expr)),
                };
                if let Some(index) = index {
                    let returns = self.function_returns.last();
                    self.found = returns.and_then(|returns| returns.get(index)).cloned().map(ExpectedValue::stored);
                }
            }
            _ => {}
        }
        if self.found.is_none() {
            visit::walk_stmt(self, stmt);
        }
    }

    fn visit_func_body(&mut self, func: &'c FuncBody) {
        let returns = self.documented.remove(&func.params_span.start).unwrap_or_default();
        self.function_returns.push(returns);
        visit::walk_func_body(self, func);
        self.function_returns.pop();
    }

    fn visit_expr(&mut self, expr: &'c Expr) {
        if self.found.is_some() || !expr.span.contains_inclusive(self.at()) {
            return;
        }
        match &expr.kind {
            ExprKind::Binary { op: BinOp::Eq | BinOp::Ne, op_span, lhs, rhs } => {
                let other = match self.place {
                    Place::After(at) => (op_span.end == at).then_some(lhs),
                    Place::String(_) if self.is_value(rhs) => Some(lhs),
                    Place::String(_) => self.is_value(lhs).then_some(rhs),
                };
                if let Some(other) = other {
                    self.found = Some(ExpectedValue { ty: self.declared.of(other), compared: true });
                }
            }
            ExprKind::Table(fields) => {
                let mut entries = entries(self.infer, fields).into_iter();
                let entry = entries.find(|(_, _, value)| value.is_some_and(|value| self.is_value(value)));
                if let Some((key, ..)) = entry {
                    self.found = self.field(expr, &key).map(ExpectedValue::stored);
                }
            }
            _ => {}
        }
        if self.found.is_none() {
            visit::walk_expr(self, expr);
        }
    }
}
