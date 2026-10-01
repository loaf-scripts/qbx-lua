//! `discard-returns`: a call on a line of its own drops the values of a function marked
//! `@nodiscard`, such as `tostring(value)` or `vector3(x, y, z)`, which then does nothing. Of a
//! function with `@overload`s, the signature the call picks decides, as for LuaLS: an overload is
//! not marked, so `math.random()` may warm up the generator. A call that may run a function without
//! the tag, such as one of several definitions, is left alone.

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use super::arguments::definitions;
use crate::infer::Infer;

/// Each call statement of `chunk` whose function is marked `@nodiscard`, with the message naming it.
pub fn discarded_returns(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let mut finder = Finder { infer, out: Vec::new() };
    finder.visit_block(&chunk.block);
    finder.out
}

struct Finder<'a, 'b> {
    infer: &'a Infer<'b>,
    out: Vec<(Span, String)>,
}

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        if let StmtKind::Expr(call) = &stmt.kind {
            let callee = match &call.kind {
                ExprKind::Call { callee, args, .. } => Some((&**callee, None, args)),
                ExprKind::MethodCall { base, method, args, .. } => Some((&**base, Some(method), args)),
                _ => None,
            };
            if let Some((base, method, args)) = callee {
                let functions = definitions(self.infer, base, method);
                let marked = |fun| {
                    let (signatures, picked) = self.infer.call_signatures(fun, args, method.is_some(), base.span.start);
                    signatures[picked].nodiscard
                };
                if !functions.is_empty() && functions.iter().all(marked) {
                    let name = match (method, base.dotted_path()) {
                        (Some(method), Some(owner)) => format!("`{owner}:{}`", method.text),
                        (Some(method), None) => format!("`{}`", method.text),
                        (None, Some(path)) => format!("`{path}`"),
                        (None, None) => "this function".to_string(),
                    };
                    self.out.push((call.span, format!("The values that {name} returns cannot be discarded")));
                }
            }
        }
        visit::walk_stmt(self, stmt);
    }
}
