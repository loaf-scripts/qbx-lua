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
//! a callee passes to a callback, such as `resolve` of `fun(resolve: fun(value: T))`, takes and
//! returns what the other arguments of that call declare for its generics, as the `boolean` of
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
        let signatures = signatures(self.infer, &self.declared, call, base, method, args);
        if signatures.is_empty() {
            return;
        }
        let values: Vec<Type> = args.iter().map(|arg| self.value(arg)).collect();
        let signatures: Vec<&FunType> = signatures.iter().map(Arc::as_ref).collect();
        self.compare(&signatures, method.is_some(), args, &values);
    }

    /// The declared type of an argument, or the type a `--[[@as T]]` right after it gives it.
    fn value(&self, arg: &Expr) -> Type {
        self.infer.cast_after(arg.span.end).unwrap_or_else(|| self.declared.of(arg))
    }

    /// Reports the arguments that the signature closest to taking them does not take, unless one of
    /// them takes them all. Only the signatures that take as many arguments as the call passes count,
    /// unless none does.
    fn compare(&mut self, signatures: &[&FunType], via_colon: bool, args: &[Expr], values: &[Type]) {
        let mut closest: Option<Vec<(Span, String)>> = None;
        for fun in candidates(self.infer, signatures, via_colon, args) {
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

    /// Each of `args` that the parameter of `fun` it goes to does not take.
    fn mismatches(&self, fun: &FunType, via_colon: bool, args: &[Expr], values: &[Type]) -> Vec<(Span, String)> {
        let passed = args.iter().zip(values).enumerate();
        passed
            .filter_map(|(i, (arg, given))| {
                let message = mismatch(&self.classes, fun, param_for(fun, via_colon, i)?, given)?;
                Some((arg.span, message))
            })
            .collect()
    }
}

/// The signatures that a call of `base`, or of its `method`, with `args` may use: those of each
/// function `definitions` finds, with their `@overload`s for the side of the call. None for a call
/// without arguments, for a native, whose arguments the runtime converts, and for
/// `require 'name' --[[@as T]]`, where the cast after the argument is that of the call.
pub fn signatures(
    infer: &Infer,
    declared: &Declared,
    call: &Expr,
    base: &Expr,
    method: Option<&Name>,
    args: &[Expr],
) -> Vec<Arc<FunType>> {
    if args.is_empty() || (method.is_none() && declared.is_native(base, 0)) {
        return Vec::new();
    }
    if args.last().is_some_and(|arg| arg.span.end == call.span.end) && infer.cast_after(call.span.end).is_some() {
        return Vec::new();
    }
    let via_colon = method.is_some();
    let at = base.span.start;
    definitions(infer, base, method).iter().flat_map(|fun| infer.call_signatures(fun, args, via_colon, at).0).collect()
}

/// The `signatures` that take as many arguments as a call passes with `args`, or all of them when
/// none does.
pub fn candidates<'f>(infer: &Infer, signatures: &[&'f FunType], via_colon: bool, args: &[Expr]) -> Vec<&'f FunType> {
    let alias = |name: &str| infer.index.alias(name, infer.side()).map(|(_, alias)| &alias.ty);
    // A call or `...` at the end passes as many values as it gives.
    let open_ended = args.last().is_some_and(Expr::is_multi_value);
    let takes_count = |fun: &FunType| {
        most_arguments(fun, via_colon).is_none_or(|most| args.len() <= most)
            && (open_ended || args.len() >= Requirement::of(fun, via_colon, &alias).arguments)
    };
    let candidates: Vec<&FunType> = signatures.iter().copied().filter(|fun| takes_count(fun)).collect();
    if candidates.is_empty() {
        signatures.to_vec()
    } else {
        candidates
    }
}

/// The parameter of `fun` that argument `index` of a call goes to. A `:` call passes its receiver to
/// the first parameter of a function not defined with `:`, as Lua does, and a `.` call passes the
/// `self` of one defined with `:` first, which no parameter lists.
pub fn param_for(fun: &FunType, via_colon: bool, index: usize) -> Option<&Param> {
    let (skip_params, skip_args) = match (via_colon, fun.is_method) {
        (true, false) => (1, 0),
        (false, true) => (0, 1),
        _ => (0, 0),
    };
    let params = fun.params.get(skip_params..).unwrap_or_default();
    match params.get(index.checked_sub(skip_args)?) {
        Some(param) if param.name != "..." => Some(param),
        _ => params.last().filter(|param| param.name == "..."),
    }
}

/// The type of the values `param` takes, one by one for a `...`.
pub fn expected(param: &Param) -> &Type {
    match &param.ty {
        Type::Variadic(inner) => inner,
        ty => ty,
    }
}

/// The message for a value of type `given` that `param` of `fun` does not take.
pub fn mismatch(classes: &Classes, fun: &FunType, param: &Param, given: &Type) -> Option<String> {
    let expected = expected(param);
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
    let from = classes.file();
    if !classes.rejects(expected, from, &given) {
        return None;
    }
    let shown = if classes.literal_mismatch(expected, from, &given) { given } else { given.widen() };
    Some(format!("Cannot assign `{shown}` to parameter `{}` of type `{expected}`", param.name))
}

/// Whether `param` takes `nil`: it is optional, or its type allows `nil` or any value, as a generic
/// does.
pub fn takes_nil(classes: &Classes, param: &Param) -> bool {
    param.optional || !classes.rejects(expected(param), classes.file(), &Type::Nil)
}

/// Every function a call of `base`, or of its `method`, may run: each member of that name defined
/// on the side of the call, each global of that name its file sees, or else the function the callee
/// holds. The functions a resource registers with `exports` come as the exports proxy calls them,
/// which drops the receiver of `exports.name:Fn()`.
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
        let found: Vec<_> = infer
            .members_named(&owner, name)
            .into_iter()
            .filter(|member| member.location.as_ref().is_none_or(|(file, _)| reaches(*file)))
            .collect();
        if !found.iter().all(|member| known(member.location.as_ref().map(|(file, _)| *file))) {
            return Vec::new();
        }
        found.into_iter().filter_map(|member| member.ty.as_fun().cloned()).collect()
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
            // declares, with the generics that the other arguments of that call declare, and is not
            // checked where the callee declares no single function for it.
            Some(Resolved::Local(id)) => {
                let declared = Declared::new(infer);
                match infer.declared_callback_param(id, |arg| declared.of(arg)) {
                    Some(callback) => infer.fun_of(&callback).into_iter().collect(),
                    None => held(),
                }
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
