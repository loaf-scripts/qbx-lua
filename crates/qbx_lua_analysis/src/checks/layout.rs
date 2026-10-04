//! Checks of how code is laid out over lines: a call whose arguments start a line of their own, and
//! whitespace at the end of a line.

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{LineIndex, Span, TokenKind};

use super::{FileInput, Sink};
use crate::diagnostic::{Fix, TextEdit};
use crate::rules;

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    let calls = sink.enabled(rules::NEWLINE_CALL).is_some();
    let spaces = sink.enabled(rules::TRAILING_SPACE).is_some();
    if !(calls || spaces) {
        return;
    }
    let lines = LineIndex::new(input.source);
    if calls {
        NewlineCalls { source: input.source, lines: &lines, sink: &mut *sink }.visit_block(&input.chunk.block);
    }
    if spaces {
        trailing_spaces(input, &lines, sink);
    }
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

/// `trailing-space`: spaces or tabs at the end of a line, as lua-language-server reports them. Those
/// inside a comment or a string, such as a long string that spans lines, belong to it and are left
/// alone, as the formatter leaves them.
fn trailing_spaces(input: &FileInput, lines: &LineIndex, sink: &mut Sink) {
    let source = input.source;
    // The comments and the strings of the file, in the order they are written.
    let comments = input.chunk.comments.iter().map(|comment| comment.span);
    let strings =
        input.chunk.tokens.iter().filter(|token| matches!(token.kind, TokenKind::String | TokenKind::LongString));
    let mut kept: Vec<Span> = comments.chain(strings.map(|token| token.span)).collect();
    kept.sort_by_key(|span| span.start);
    let is_kept = |offset: u32| {
        let next = kept.partition_point(|span| span.start <= offset);
        next > 0 && kept[next - 1].contains(offset)
    };
    for line in 0..lines.line_count() {
        let start = lines.line_start(line) as usize;
        let text = source[start..].split('\n').next().unwrap_or_default();
        let text = text.strip_suffix('\r').unwrap_or(text);
        let code = text.trim_end_matches([' ', '\t']);
        if code.len() == text.len() {
            continue;
        }
        let span = Span::new((start + code.len()) as u32, (start + text.len()) as u32);
        if is_kept(span.end - 1) {
            continue;
        }
        let message = if code.is_empty() { "line contains only whitespace" } else { "trailing whitespace" };
        let fix =
            Fix { title: "Remove trailing whitespace".into(), edits: vec![TextEdit { span, new_text: String::new() }] };
        sink.report_with(rules::TRAILING_SPACE, span, message, None, Some(fix));
    }
}
