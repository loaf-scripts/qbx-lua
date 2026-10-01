//! `param-type-mismatch`: a call passes an argument that its parameter does not take, such as a
//! string for `number`. As for `assign-type-mismatch`, the argument has to be a different kind of
//! value, or a literal the parameter does not list, and as for `impossible-comparison` only
//! declared types count: what inference reads from assigned values is left out.
//!
//! A call is compared with every function it may run: each member of that name on its side, such
//! as the client and the server `lib.notify`, or each global of that name its file sees, with their
//! `@overload`s for its side. It passes when a signature that takes as many arguments as it passes
//! takes each of them; only when none takes that many do the others count. The payload of a
//! `---@callback` wrapper call is compared with the handlers registered under its name.
//!
//! Natives are left out: the runtime converts their arguments, so `0` passes for a `boolean`, a
//! string for a hash and a vector for three floats. `nil` is never a mismatch, as annotations often
//! leave out the `?` of a parameter that code skips, and neither is `false`, which FiveM code passes
//! to skip a parameter of exports and events, whose arguments are serialized. Parameters typed with a
//! generic of the function called are left out, as the arguments of the call bind them. The function
//! a callee passes to a callback, such as `resolve` of `fun(resolve: fun(value: T))`, takes what the
//! other arguments of that call declare for its generics, as the `boolean` of
//! `Promise:New('boolean', function(resolve) end)` for a `` `T` ``; a generic they leave unbound
//! takes any value.

use std::sync::Arc;

use qbx_lua_analysis::scope::Resolved;
use qbx_lua_analysis::signature::{most_arguments, Requirement};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{SmolStr, Span};

use super::callback_payloads::Payload;
use super::class_tables::Classes;
use super::comparisons::Declared;
use crate::index::{FileId, FileOrigin};
use crate::infer::{Infer, NATIVE_HANDLE_TYPES};
use crate::types::{FunType, Param, Type};

/// Each argument whose parameter does not take it, with the message naming both types, also among
/// the `payloads` of the wrapper calls of `chunk`.
pub fn mismatched_arguments(infer: &Infer, chunk: &Chunk, payloads: &[Payload]) -> Vec<(Span, String)> {
    let mut calls = Calls { infer, declared: Declared::new(infer), classes: Classes::new(infer), out: Vec::new() };
    calls.visit_block(&chunk.block);
    for payload in payloads {
        let values: Vec<Type> = payload.args.iter().map(|arg| calls.value(arg)).collect();
        let handlers: Vec<&FunType> = payload.handlers.iter().collect();
        calls.compare(&handlers, false, payload.args, &values);
    }
    calls.out
}

struct Calls<'a, 'b> {
    infer: &'a Infer<'b>,
    declared: Declared<'a, 'b>,
    classes: Classes<'a, 'b>,
    out: Vec<(Span, String)>,
}

impl<'c> Visitor<'c> for Calls<'_, '_> {
    fn visit_expr(&mut self, expr: &'c Expr) {
        match &expr.kind {
            ExprKind::Call { callee, args, .. } => self.check(expr, callee, None, args),
            ExprKind::MethodCall { base, method, args, .. } => self.check(expr, base, Some(method), args),
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

impl Calls<'_, '_> {
    fn check(&mut self, call: &Expr, base: &Expr, method: Option<&Name>, args: &[Expr]) {
        if args.is_empty() || (method.is_none() && self.declared.is_native(base, 0)) {
            return;
        }
        // In `require 'name' --[[@as T]]`, the cast after the argument is that of the call.
        if args.last().is_some_and(|arg| arg.span.end == call.span.end)
            && self.infer.cast_after(call.span.end).is_some()
        {
            return;
        }
        let via_colon = method.is_some();
        let at = base.span.start;
        let signatures: Vec<Arc<FunType>> = definitions(self.infer, base, method)
            .iter()
            .flat_map(|fun| self.infer.call_signatures(fun, args, via_colon, at).0)
            .collect();
        if signatures.is_empty() {
            return;
        }
        let values: Vec<Type> = args.iter().map(|arg| self.value(arg)).collect();
        let signatures: Vec<&FunType> = signatures.iter().map(Arc::as_ref).collect();
        self.compare(&signatures, via_colon, args, &values);
    }

    /// The declared type of an argument, or the type a `--[[@as T]]` right after it gives it.
    fn value(&self, arg: &Expr) -> Type {
        self.infer.cast_after(arg.span.end).unwrap_or_else(|| self.declared.of(arg))
    }

    /// Reports the arguments that the signature closest to taking them does not take, unless one of
    /// them takes them all. Only the signatures that take as many arguments as the call passes count,
    /// unless none does.
    fn compare(&mut self, signatures: &[&FunType], via_colon: bool, args: &[Expr], values: &[Type]) {
        let infer = self.infer;
        let alias = |name: &str| infer.index.alias(name, infer.side()).map(|(_, alias)| &alias.ty);
        // A call or `...` at the end passes as many values as it gives.
        let open_ended = args.last().is_some_and(Expr::is_multi_value);
        let takes_count = |fun: &FunType| {
            most_arguments(fun, via_colon).is_none_or(|most| args.len() <= most)
                && (open_ended || args.len() >= Requirement::of(fun, via_colon, &alias).arguments)
        };
        let mut candidates: Vec<&FunType> = signatures.iter().copied().filter(|fun| takes_count(fun)).collect();
        if candidates.is_empty() {
            candidates = signatures.to_vec();
        }
        let mut closest: Option<Vec<(Span, String)>> = None;
        for fun in candidates {
            let found = self.mismatches(fun, via_colon, args, values);
            if found.is_empty() {
                return;
            }
            if closest.as_ref().is_none_or(|closest| found.len() < closest.len()) {
                closest = Some(found);
            }
        }
        self.out.extend(closest.unwrap_or_default());
    }

    /// Each of `args` that the parameter of `fun` it goes to does not take. A `:` call passes its
    /// receiver to the first parameter of a function not defined with `:`, as Lua does, and a `.`
    /// call passes the `self` of one defined with `:` first.
    fn mismatches(&self, fun: &FunType, via_colon: bool, args: &[Expr], values: &[Type]) -> Vec<(Span, String)> {
        let (skip_params, skip_args) = match (via_colon, fun.is_method) {
            (true, false) => (1, 0),
            (false, true) => (0, 1),
            _ => (0, 0),
        };
        let params = fun.params.get(skip_params..).unwrap_or_default();
        let rest = params.last().filter(|param| param.name == "...");
        let mut out = Vec::new();
        for (i, (arg, given)) in args.iter().zip(values).enumerate().skip(skip_args) {
            let param = match params.get(i - skip_args) {
                Some(param) if param.name != "..." => param,
                _ => match rest {
                    Some(rest) => rest,
                    None => break,
                },
            };
            if let Some(message) = self.mismatch(fun, param, given) {
                out.push((arg.span, message));
            }
        }
        out
    }

    /// The message for a value of type `given` that `param` does not take.
    fn mismatch(&self, fun: &FunType, param: &Param, given: &Type) -> Option<String> {
        let expected = match &param.ty {
            Type::Variadic(inner) => inner,
            ty => ty,
        };
        let given = given.without_nil();
        let is_generic = |name: &str| fun.generics.iter().any(|generic| generic == name);
        // Resources declare classes such as `Vehicle` that share the name of a native handle, which a
        // parameter of that type may well mean.
        let is_handle = |name: &str| NATIVE_HANDLE_TYPES.contains(&name);
        if matches!(given, Type::Unknown | Type::Nil | Type::BooleanLit(false))
            || mentions(expected, &is_generic)
            || names(expected, &is_handle)
        {
            return None;
        }
        let from = self.classes.file();
        if !self.classes.rejects(expected, from, &given) {
            return None;
        }
        let shown = if self.classes.literal_mismatch(expected, from, &given) { given } else { given.widen() };
        Some(format!("Cannot assign `{shown}` to parameter `{}` of type `{expected}`", param.name))
    }
}

/// Every function a call of `base`, or of its `method`, may run: each member of that name defined
/// on the side of the call, each global of that name its file sees, or else the function the callee
/// holds. The exports proxy drops the receiver of `exports.name:Fn()`, so those calls pass their
/// arguments to the parameters that the function lists.
///
/// In an escrowed resource, an encrypted script may define the globals a call reaches differently,
/// so a call through a global is left out unless only the runtime stubs define what it reaches, or
/// it goes through the exports proxy.
pub fn definitions(infer: &Infer, base: &Expr, method: Option<&Name>) -> Vec<Arc<FunType>> {
    let side = infer.side_at(base.span.start);
    let reaches = |file: FileId| match (infer.index.file(file).and_then(|f| f.side), side) {
        (Some(defined), Some(call)) => defined.is_available_on(call),
        _ => true,
    };
    let hidden = in_escrowed_resource(infer) && global_root(infer, base).is_some_and(|root| root.text != "exports");
    let known = |file: Option<FileId>| {
        !hidden || file.and_then(|file| infer.index.file(file)).is_some_and(|f| f.origin == FileOrigin::Stub)
    };
    let members = |owner: &Expr, name: &str| -> Vec<Arc<FunType>> {
        let owner = infer.expr(owner);
        let exported = matches!(owner, Type::Exports(Some(_)));
        let found: Vec<_> = infer
            .members_named(&owner, name)
            .into_iter()
            .filter(|member| member.location.as_ref().is_none_or(|(file, _)| reaches(*file)))
            .collect();
        if !found.iter().all(|member| known(member.location.as_ref().map(|(file, _)| *file))) {
            return Vec::new();
        }
        found
            .into_iter()
            .filter_map(|member| member.ty.as_fun().cloned())
            .map(|fun| if exported { without_receiver(&fun) } else { fun })
            .collect()
    };
    let held = || infer.callee_fun(base, method).map(|(fun, _)| fun).into_iter().collect();
    match (method, &base.unparen().kind) {
        (Some(method), _) => members(base, &method.text),
        (None, ExprKind::Field { base: owner, name, .. }) => members(owner, &name.text),
        (None, ExprKind::Index { base: owner, index, .. }) if index.as_string().is_some() => {
            members(owner, index.as_string().map(SmolStr::as_str).unwrap_or_default())
        }
        (None, ExprKind::Name(name)) => match infer.ctx.resolution.resolve_at(name.span.start) {
            Some(Resolved::Global(_)) => {
                let globals = infer.index.globals_named(&name.text, infer.ctx.file);
                let globals: Vec<_> = globals.into_iter().filter(|(file, _)| reaches(*file)).collect();
                if !globals.iter().all(|(file, _)| known(Some(*file))) {
                    return Vec::new();
                }
                globals.into_iter().filter_map(|(_, symbol)| symbol.ty.as_fun().cloned()).collect()
            }
            // A parameter whose function the callee of its own function gives takes what the callee
            // declares, with the generics that the other arguments of that call declare.
            Some(Resolved::Local(id)) => {
                let declared = Declared::new(infer);
                let callback = infer.declared_callback_param(id, |arg| declared.of(arg));
                callback.and_then(|ty| infer.fun_of(&ty)).map_or_else(held, |fun| vec![fun])
            }
            _ => held(),
        },
        _ => held(),
    }
}

/// Whether the file being checked belongs to an escrowed resource.
fn in_escrowed_resource(infer: &Infer) -> bool {
    let resource = infer.index.file(infer.ctx.file).and_then(|file| file.resource);
    resource.and_then(|id| infer.index.resource(id)).is_some_and(|resource| resource.escrowed)
}

/// The global that the path `expr` starts with, as `Utils` of `Utils.round`.
fn global_root<'e>(infer: &Infer, expr: &'e Expr) -> Option<&'e Name> {
    match &expr.unparen().kind {
        ExprKind::Name(name) => {
            matches!(infer.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_))).then_some(name)
        }
        ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => global_root(infer, base),
        _ => None,
    }
}

/// `fun` as called through the exports proxy, which passes the values after the receiver of a `:`
/// call, and drops the first value of a `.` call: like a function defined with `:`, unless it lists
/// `self` itself.
fn without_receiver(fun: &Arc<FunType>) -> Arc<FunType> {
    let plain = |fun: &FunType| {
        let lists_self = fun.params.first().is_some_and(|param| param.name == "self");
        FunType { is_method: !lists_self, ..fun.clone() }
    };
    let mut out = plain(fun);
    out.overloads = fun.overloads.iter().map(|overload| Arc::new(plain(overload))).collect();
    Arc::new(out)
}

/// Whether a named type whose name `matches` is part of `ty`, as `T` is of `T[]`.
fn mentions(ty: &Type, matches: &impl Fn(&str) -> bool) -> bool {
    match ty {
        Type::Named(name, args) => matches(name) || args.iter().any(|arg| mentions(arg, matches)),
        Type::Array(inner) | Type::Variadic(inner) => mentions(inner, matches),
        Type::Map(key, value) => mentions(key, matches) || mentions(value, matches),
        Type::Tuple(parts) | Type::Union(parts) => parts.iter().any(|part| mentions(part, matches)),
        Type::Shape(shape) => shape.fields.iter().any(|field| mentions(&field.ty, matches)),
        Type::Fun(fun) => {
            fun.params.iter().any(|param| mentions(&param.ty, matches))
                || fun.returns.iter().any(|ty| mentions(ty, matches))
        }
        _ => false,
    }
}

/// Whether `ty` is a named type whose name `matches`, or a union with one.
fn names(ty: &Type, matches: &impl Fn(&str) -> bool) -> bool {
    match ty {
        Type::Named(name, args) => args.is_empty() && matches(name),
        Type::Union(parts) => parts.iter().any(|part| names(part, matches)),
        _ => false,
    }
}
