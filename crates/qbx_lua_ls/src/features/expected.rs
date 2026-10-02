//! The type that a value has to be where it is written, which completion offers the values of:
//! the `---@type` above the `local` or assignment that stores it, the declared type of the local
//! or class field it is assigned to, the `@field` it sets in a class-typed table, the `@return` of
//! the function that returns it, and the declared type of what `==` or `~=` compares it with.

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;
use rustc_hash::FxHashMap;

use qbx_lua_analysis::scope::Resolved;

use super::class_tables::{class_tables, entries, Classes, Key};
use super::comparisons::Declared;
use crate::index::FileId;
use crate::infer::{Decl, Infer};
use crate::types::{DescribedValue, Type};

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
    /// What the `---|` lines under the annotation that declares `ty` say about the values they list.
    pub values: Vec<DescribedValue>,
}

impl ExpectedValue {
    fn stored(ty: Type, values: Vec<DescribedValue>) -> Self {
        Self { ty, compared: false, values }
    }
}

/// The types of the values a function returns, each with the values the `---|` lines under its
/// `@return` describe.
type Returns = Vec<(Type, Vec<DescribedValue>)>;

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
    function_returns: Vec<Returns>,
    /// The `@return` types of the functions whose doc comment was visited, by the start of their
    /// parameter list.
    documented: FxHashMap<u32, Returns>,
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
    fn field(&self, table: &Expr, key: &Key) -> Option<ExpectedValue> {
        let tables = class_tables(self.infer, self.infer.ctx.chunk);
        let found = tables.into_iter().find(|found| found.table.span == table.span)?;
        let classes = Classes::new(self.infer);
        let (ty, _) = classes.field_type(&found.class, &found.args, found.from, key)?;
        let values = match key {
            Key::Name(name) => field_values(&classes, &found.class, &found.args, found.from, name),
            Key::Typed(_) => Vec::new(),
        };
        Some(ExpectedValue::stored(ty, values))
    }

    /// What the `---|` lines that declare the values of `expr` say about them: those under the
    /// `---@type` or `@param` of a local, the `@return` of the call that gives it, or the `@field`
    /// of the class that `expr` reads it from.
    fn described(&self, expr: &Expr) -> Vec<DescribedValue> {
        let ctx = self.infer.ctx;
        match &expr.unparen().kind {
            ExprKind::Name(name) => {
                let Some(Resolved::Local(id)) = ctx.resolution.resolve_at(name.span.start) else { return Vec::new() };
                match ctx.decl(ctx.resolution.local(id).decl.start) {
                    Some(Decl::Local { stmt, index }) => {
                        let doc = ctx.doc_at(stmt.span.start);
                        let StmtKind::Local { exprs, .. } = &stmt.kind else { return Vec::new() };
                        if doc.type_at(*index).is_some() {
                            return doc.type_values_at(*index).to_vec();
                        }
                        match (exprs.get(*index), exprs.last()) {
                            (Some(value), _) => self.described(value),
                            (None, Some(last)) if last.is_multi_value() => self.returned(last, index + 1 - exprs.len()),
                            (None, _) => Vec::new(),
                        }
                    }
                    Some(Decl::Param { func, index, doc_anchor: Some(anchor), .. }) => {
                        let name = &func.params[*index].text;
                        let doc = ctx.doc_at(*anchor);
                        doc.params
                            .iter()
                            .find(|param| param.name == *name)
                            .map(|param| param.values.clone())
                            .unwrap_or_default()
                    }
                    _ => Vec::new(),
                }
            }
            ExprKind::Field { base, name, .. } => self.field_described(base, &name.text),
            ExprKind::Index { base, index, .. } => match index.as_string() {
                Some(name) => self.field_described(base, name),
                None => Vec::new(),
            },
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => self.returned(expr, 0),
            _ => Vec::new(),
        }
    }

    /// The described values of the `@field` called `name` of the class that `base` is.
    fn field_described(&self, base: &Expr, name: &str) -> Vec<DescribedValue> {
        let classes = Classes::new(self.infer);
        let from = classes.file();
        match classes.class_of(&self.infer.expr(base), from) {
            Some((class, args, view)) => field_values(&classes, &class, &args, view, name),
            None => Vec::new(),
        }
    }

    /// The described values of the value at `index` of those `call` returns, as the signature it
    /// picks declares them.
    fn returned(&self, call: &Expr, index: usize) -> Vec<DescribedValue> {
        let (base, method, args) = match &call.kind {
            ExprKind::Call { callee, args, .. } => (callee, None, args),
            ExprKind::MethodCall { base, method, args, .. } => (base, Some(method), args),
            _ => return Vec::new(),
        };
        let Some((fun, _)) = self.infer.callee_fun(base, method) else { return Vec::new() };
        let fun = self.infer.call_signature(&fun, args, method.is_some(), base.span.start);
        fun.return_values.get(index).cloned().unwrap_or_default()
    }
}

/// The described values of the `@field` called `name` of `class`, or of a parent, as `from` sees it.
fn field_values(classes: &Classes, class: &str, args: &[Type], from: FileId, name: &str) -> Vec<DescribedValue> {
    let mut fields = classes.fields(class, args, from).into_iter();
    fields.find(|field| field.name == name).map(|field| field.values).unwrap_or_default()
}

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        if self.found.is_some() || !stmt.span.contains_inclusive(self.at()) {
            return;
        }
        for (doc, functions) in self.infer.ctx.function_docs(stmt) {
            for func in functions {
                let returns =
                    doc.returns.iter().map(|r| (self.infer.doc_type_for(stmt, func, &r.ty), r.values.clone()));
                self.documented.insert(func.params_span.start, returns.collect());
            }
        }
        match &stmt.kind {
            StmtKind::Local { exprs, .. } => {
                if let Some(index) = exprs.iter().position(|expr| self.is_value(expr)) {
                    let doc = self.infer.ctx.doc_at(stmt.span.start);
                    // `---@class Name` above a table declares the class rather than an instance of it.
                    if doc.declared_class().is_none() {
                        let values = doc.type_values_at(index).to_vec();
                        self.found = doc.type_at(index).map(|ty| ExpectedValue::stored(ty.clone(), values));
                    }
                }
            }
            StmtKind::Assign { targets, exprs } => {
                if let Some(index) = exprs.iter().position(|expr| self.is_value(expr)) {
                    let doc = self.infer.ctx.doc_at(stmt.span.start);
                    self.found = match doc.type_at(index) {
                        Some(ty) => Some(ExpectedValue::stored(ty.clone(), doc.type_values_at(index).to_vec())),
                        None => targets
                            .get(index)
                            .map(|target| ExpectedValue::stored(self.declared.of(target), self.described(target))),
                    };
                }
            }
            StmtKind::Return(exprs) => {
                let index = match self.place {
                    Place::After(at) => (stmt.span.start + "return".len() as u32 == at).then_some(0),
                    Place::String(_) => exprs.iter().position(|expr| self.is_value(expr)),
                };
                if let Some(index) = index {
                    let returned = self.function_returns.last().and_then(|returns| returns.get(index)).cloned();
                    self.found = returned.map(|(ty, values)| ExpectedValue::stored(ty, values));
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
                    let (ty, values) = (self.declared.of(other), self.described(other));
                    self.found = Some(ExpectedValue { ty, compared: true, values });
                }
            }
            ExprKind::Table(fields) => {
                let mut entries = entries(self.infer, fields).into_iter();
                let entry = entries.find(|(_, _, value)| value.is_some_and(|value| self.is_value(value)));
                if let Some((key, ..)) = entry {
                    self.found = self.field(expr, &key);
                }
            }
            _ => {}
        }
        if self.found.is_none() {
            visit::walk_expr(self, expr);
        }
    }
}
