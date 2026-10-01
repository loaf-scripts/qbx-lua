//! `missing-parameter` and `redundant-parameter` for the payload of a `---@callback await` or
//! `trigger` wrapper call: the values passed in its `...` go to the handler registered under the
//! name it passes, so they have to cover that handler's required parameters, after the player a
//! handler outside the client receives first, and the handler has to take them all. qbx-lint
//! checks the wrapper's own parameters, but only the language server knows which handler a name
//! reaches.

use qbx_lua_analysis::signature::{most_arguments, Requirement};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::Span;

use crate::callback_wrappers::{self, Wrapper};
use crate::index::{EventFamily, EventKind};
use crate::infer::Infer;
use crate::types::{CallbackRole, FunType};

/// A call to an `await` or `trigger` wrapper that names a handler.
pub struct Payload<'c> {
    /// The callback name the call passes.
    pub name: &'c str,
    /// Where the call names the wrapper.
    pub span: Span,
    /// The values passed in the wrapper's `...`.
    pub args: &'c [Expr],
    /// The parameters of each handler registered under the name, without the player a handler
    /// outside the client receives first.
    pub handlers: Vec<FunType>,
}

/// Each call of `chunk` to an `await` or `trigger` wrapper whose name reaches a handler.
pub fn payloads<'c>(infer: &Infer, chunk: &'c Chunk) -> Vec<Payload<'c>> {
    let has_handlers = infer.index.events().any(|(_, event)| {
        matches!(event.family, EventFamily::Custom(_)) && event.kind == EventKind::Callback && event.handler.is_some()
    });
    if !has_handlers {
        return Vec::new();
    }
    let mut calls = Calls { infer, out: Vec::new() };
    calls.visit_block(&chunk.block);
    calls.out
}

/// Each call to an `await` or `trigger` wrapper whose payload leaves out a parameter the handler
/// requires, with the message naming it. Any of the handlers may be the one that runs, so the one
/// that needs the fewest values decides.
pub fn missing_payloads(infer: &Infer, payloads: &[Payload]) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    for payload in payloads {
        // A call or `...` at the end passes as many values as it gives.
        if payload.args.last().is_some_and(Expr::is_multi_value) {
            continue;
        }
        let passed = payload.args.len();
        let target = callback_wrappers::target_of(infer.side_at(payload.span.start));
        let alias = |name: &str| infer.index.alias(name, target).map(|(_, alias)| &alias.ty);
        let Some(least) = payload
            .handlers
            .iter()
            .map(|handler| Requirement::of(handler, false, &alias))
            .min_by_key(|requirement| requirement.arguments)
            .filter(|least| passed < least.arguments)
        else {
            continue;
        };
        let Some(param) = least.first_missing(passed) else { continue };
        let message = format!(
            "Callback '{}' is called with {passed} argument{}, but needs {}; '{}' ({}) will be nil",
            payload.name,
            plural(passed),
            least.arguments,
            param.name,
            param.ty
        );
        out.push((payload.span, message));
    }
    out
}

/// Each call to an `await` or `trigger` wrapper whose payload passes more values than the handler
/// takes, at the values it does not take. The handler that takes the most decides.
pub fn redundant_payloads(payloads: &[Payload]) -> Vec<(Span, String)> {
    let mut out = Vec::new();
    'payloads: for payload in payloads {
        let mut most = 0;
        for handler in &payload.handlers {
            let Some(count) = most_arguments(handler, false) else { continue 'payloads };
            most = most.max(count);
        }
        let (Some(first), Some(last)) = (payload.args.get(most), payload.args.last()) else { continue };
        let passed = payload.args.len();
        let message = format!(
            "Callback '{}' is called with {passed} argument{}, but its handler takes at most {most}",
            payload.name,
            plural(passed)
        );
        out.push((first.span.to(last.span), message));
    }
    out
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

struct Calls<'a, 'b, 'c> {
    infer: &'a Infer<'b>,
    out: Vec<Payload<'c>>,
}

impl<'c> Visitor<'c> for Calls<'_, '_, 'c> {
    fn visit_expr(&mut self, expr: &'c Expr) {
        match &expr.kind {
            ExprKind::Call { callee, args, .. } => {
                let span = match &callee.kind {
                    ExprKind::Field { name, .. } => name.span,
                    ExprKind::Index { index, .. } => index.span,
                    _ => callee.span,
                };
                self.check(expr, callee, None, args, span);
            }
            ExprKind::MethodCall { base, method, args, .. } => self.check(expr, base, Some(method), args, method.span),
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

impl<'c> Calls<'_, '_, 'c> {
    fn check(&mut self, call: &Expr, base: &Expr, method: Option<&Name>, args: &'c [Expr], span: Span) {
        // Without a literal name there is no handler to find, and most calls pass none.
        if !args.iter().any(|arg| arg.as_string().is_some()) {
            return;
        }
        let Some((fun, _)) = self.infer.callee_fun(base, method) else { return };
        let Some(wrapper) = Wrapper::of(&fun, method.is_some()) else { return };
        let Some(payload) = wrapper.payload.filter(|_| wrapper.tag.role != CallbackRole::Register) else { return };
        let Some(name) = args.get(wrapper.arg(wrapper.name)).and_then(Expr::as_string) else { return };
        let Some(args) = args.get(wrapper.arg(payload)..) else { return };

        let target = callback_wrappers::target_of(self.infer.side_at(call.span.start));
        let handlers: Vec<FunType> = callback_wrappers::handlers(self.infer.index, &wrapper.family(), name, target)
            .into_iter()
            .filter_map(|(_, event)| {
                let handler = event.handler.as_deref()?;
                let params = handler.params[callback_wrappers::source_skip(event, handler)..].to_vec();
                Some(FunType { params, ..FunType::default() })
            })
            .collect();
        if !handlers.is_empty() {
            self.out.push(Payload { name, span, args, handlers });
        }
    }
}
