//! Checks of how code is laid out over lines: a call whose arguments start a line of their own.

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{LineIndex, Span};

use super::{FileInput, Sink};
use crate::rules;

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    if sink.enabled(rules::NEWLINE_CALL).is_none() {
        return;
    }
    let lines = LineIndex::new(input.source);
    NewlineCalls { source: input.source, lines: &lines, sink }.visit_block(&input.chunk.block);
}

/// `newline-call`: Lua reads a line that starts with `(` as the arguments of a call to whatever
/// ends the line above, so `local f = g` followed by `(h):run()` calls `g`. Like lua-language-server,
/// only arguments that could also stand as a statement of their own are reported: one expression in
/// parentheses whose result is then indexed, as by `.x`, `[k]` or `:m()`, or called, as an
/// immediately invoked function is.
struct NewlineCalls<'a, 's> {
    source: &'a str,
    lines: &'a LineIndex,
    sink: &'a mut Sink<'s>,
}

impl NewlineCalls<'_, '_> {
    fn continued(&mut self, call: &Expr) {
        let ExprKind::Call { callee, args, args_span, style: CallStyle::Paren } = &call.kind else { return };
        if args.len() != 1 || self.lines.line_of(callee.span.end) == self.lines.line_of(args_span.start) {
            return;
        }
        let text = callee.span.text(self.source);
        let callee = match text.contains('\n') || text.chars().count() > 40 {
            true => "the expression".to_string(),
            false => format!("'{text}'"),
        };
        let message = format!(
            "{callee} on the line above is called with the parentheses on this line; put a ';' before the '(' if this line starts a new statement"
        );
        self.sink.report(rules::NEWLINE_CALL, Span::new(call.span.start, args_span.end), message);
    }
}

impl<'ast> Visitor<'ast> for NewlineCalls<'_, '_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Field { base, .. } | ExprKind::Index { base, .. } | ExprKind::MethodCall { base, .. } => {
                self.continued(base)
            }
            ExprKind::Call { callee, .. } => self.continued(callee),
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}
