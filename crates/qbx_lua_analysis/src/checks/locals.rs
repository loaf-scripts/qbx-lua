use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use rustc_hash::FxHashSet;

use super::{FileInput, Sink};
use crate::diagnostic::Tag;
use crate::env::leading_doc_lines;
use crate::rules;
use crate::scope::{Local, LocalKind, LocalRef};

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    let prefix = input.config.ignore_unused_prefix.as_str();
    let mut shapes = Shapes { input, tables: FxHashSet::default(), field_sets: FxHashSet::default() };
    shapes.visit_block(&input.chunk.block);
    for local in &input.resolution.locals {
        if local.name.is_empty() {
            continue;
        }
        check_const_writes(local, sink);
        let ignored = local.name == "_" || (!prefix.is_empty() && local.name.starts_with(prefix));
        if ignored || local.kind == LocalKind::ImplicitSelf {
            continue;
        }
        check_unused(local, &shapes, sink);
        if let Some(previous) = local.redefines {
            let previous = input.resolution.local(previous);
            if previous.kind != LocalKind::Param || local.kind != LocalKind::Param {
                sink.report(
                    rules::REDEFINED_LOCAL,
                    local.decl,
                    format!("local '{}' is already declared in this scope", local.name),
                );
            }
        } else if local.shadows.is_some() {
            sink.report(rules::SHADOWED_LOCAL, local.decl, format!("local '{}' shadows an outer local", local.name));
        }
    }
}

fn check_const_writes(local: &Local, sink: &mut Sink) {
    let Some(attrib @ (Attrib::Const | Attrib::Close)) = local.attrib else { return };
    let label = if attrib == Attrib::Const { "const" } else { "close" };
    for write in local.refs.iter().filter(|r| r.write) {
        sink.report(
            rules::CONST_REASSIGN,
            write.span,
            format!("cannot assign to '{}': it is declared <{label}>", local.name),
        );
    }
}

fn check_unused(local: &Local, shapes: &Shapes, sink: &mut Sink) {
    if local.attrib == Some(Attrib::Close) {
        return;
    }
    let name = &local.name;
    if local.is_read() {
        // Like lua-language-server, setting fields of a table that is never read is no use of it.
        let sets_fields = |r: &LocalRef| shapes.field_sets.contains(&r.span.start);
        if shapes.tables.contains(&local.decl.start) && local.refs.iter().filter(|r| !r.write).all(sets_fields) {
            let message = format!("local '{name}' is never read; only its fields are set");
            sink.report_with(rules::UNUSED_LOCAL, local.decl, message, Some(Tag::Unnecessary), None);
        }
        return;
    }
    let (code, message) = match local.kind {
        LocalKind::Param => (rules::UNUSED_ARGUMENT, format!("unused argument '{name}'")),
        LocalKind::LoopVar => (rules::UNUSED_LOOP_VARIABLE, format!("unused loop variable '{name}'")),
        LocalKind::LocalFunction => (rules::UNUSED_FUNCTION, format!("unused function '{name}'")),
        LocalKind::Local if local.refs.iter().any(|r| r.write) => {
            (rules::UNUSED_LOCAL, format!("local '{name}' is assigned but never read"))
        }
        LocalKind::Local => (rules::UNUSED_LOCAL, format!("unused local '{name}'")),
        LocalKind::ImplicitSelf => return,
    };
    sink.report_with(code, local.decl, message, Some(Tag::Unnecessary), None);
}

/// What the statements of the file tell about how their locals are used.
struct Shapes<'a> {
    input: &'a FileInput<'a>,
    /// The locals declared with a table constructor, by where their name starts, other than the
    /// table a `---@class` annotation declares, whose fields its users read.
    tables: FxHashSet<u32>,
    /// The references that only set a field of the table they name: the `t` of `t.x = v`,
    /// `t[k] = v`, `function t.f()` and `function t:m()`.
    field_sets: FxHashSet<u32>,
}

impl<'ast> Visitor<'ast> for Shapes<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::Local { names, exprs, .. } => {
                let tables = names.iter().zip(exprs).filter(|(_, value)| matches!(value.kind, ExprKind::Table(_)));
                let mut tables = tables.map(|(name, _)| name.name.span.start).peekable();
                if tables.peek().is_some() && !self.declares_class(stmt) {
                    self.tables.extend(tables);
                }
            }
            StmtKind::Assign { targets, .. } => {
                for target in targets {
                    if let ExprKind::Field { base, .. } | ExprKind::Index { base, .. } = &target.kind {
                        if let ExprKind::Name(name) = &base.kind {
                            self.field_sets.insert(name.span.start);
                        }
                    }
                }
            }
            StmtKind::Function { name, .. } if name.path.len() + usize::from(name.method.is_some()) == 1 => {
                self.field_sets.insert(name.base.span.start);
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }
}

impl Shapes<'_> {
    fn declares_class(&self, stmt: &Stmt) -> bool {
        let docs = leading_doc_lines(self.input.source, &self.input.chunk.comments, stmt.span.start);
        docs.iter().any(|line| line.trim_start().starts_with("@class"))
    }
}
