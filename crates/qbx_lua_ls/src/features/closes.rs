//! `close-non-object`: a `<close>` local without a value, or with one that cannot be closed. When
//! the local goes out of scope, Lua 5.4 calls the `__close` metamethod of its value, skips `nil` and
//! `false`, and raises an error for anything else. As in lua-language-server, a value is reported
//! only when no part of its type can be closed: numbers, strings, `true`, functions, threads and
//! userdata. Tables and classes may have the metamethod, and a value of unknown type may be anything.
//! Unlike lua-language-server, `false` passes, as the runtime takes it, and aliases are looked through.

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use super::class_tables::Classes;
use crate::infer::Infer;
use crate::types::Type;

/// Each `<close>` local of `chunk` without a value, and each value of one that cannot be closed,
/// with the message.
pub fn unclosable_values(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let mut finder = Finder { classes: Classes::new(infer), out: Vec::new() };
    finder.visit_block(&chunk.block);
    finder.out
}

struct Finder<'a, 'b> {
    classes: Classes<'a, 'b>,
    out: Vec<(Span, String)>,
}

impl Finder<'_, '_> {
    /// Whether a value of type `ty` may be one that a `<close>` local takes.
    fn may_close(&self, ty: &Type) -> bool {
        let mut parts = Vec::new();
        self.classes.flatten(ty, self.classes.file(), &mut parts, 0);
        parts.is_empty()
            || parts.iter().any(|part| match part {
                Type::Variadic(inner) => self.may_close(inner),
                Type::Boolean
                | Type::BooleanLit(true)
                | Type::Number
                | Type::Integer
                | Type::IntLit(_)
                | Type::Handle(_)
                | Type::String
                | Type::StringLit(_)
                | Type::Function
                | Type::Fun(_)
                | Type::Thread
                | Type::Userdata => false,
                _ => true,
            })
    }
}

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        if let StmtKind::Local { names, exprs, in_unpack: false } = &stmt.kind {
            let closed = names.iter().enumerate().filter(|(_, name)| matches!(name.attrib, Some((Attrib::Close, _))));
            for (index, name) in closed {
                match self.classes.values(exprs).get(index) {
                    Some((ty, span)) => {
                        if !self.may_close(ty) {
                            let message = format!(
                                "Cannot close a value of type `{}`; a `<close>` local takes `nil`, `false` or a value with a `__close` metamethod",
                                ty.widen()
                            );
                            self.out.push((*span, message));
                        }
                    }
                    // A call or `...` that gives fewer values leaves `nil`.
                    None if exprs.last().is_some_and(Expr::is_multi_value) => {}
                    None => {
                        let message = format!("`{}` is declared `<close>` without a value to close", name.name.text);
                        self.out.push((name.name.span, message));
                    }
                }
            }
        }
        visit::walk_stmt(self, stmt);
    }
}
