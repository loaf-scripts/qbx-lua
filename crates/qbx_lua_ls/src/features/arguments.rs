//! `param-type-mismatch`: a call passes an argument that its parameter does not take, such as a
//! string for `number`. As for `assign-type-mismatch`, the argument has to be a different kind of
//! value, or a literal the parameter does not list, for each type a union lists, and as for
//! `impossible-comparison` only declared types count: what inference reads from assigned values is
//! left out.
//!
//! A call is compared with every function it may run: each member of that name on its side, such
//! as the client and the server `lib.notify`, or each global of that name its file sees, with their
//! `@overload`s for its side. It passes when a signature that takes as many arguments as it passes
//! takes each of them; only when none takes that many do the others count. The payload of a
//! `---@callback` wrapper call is compared with the handlers registered under its name.
//!
//! As in LuaLS, `nil`, and a value whose type allows it like a `string?`, needs a parameter that takes
//! `nil`, and `false` is a boolean like any other. Also as in LuaLS, the `nil` and `false` that a
//! value read from a field or a key, such as `self.handle` or `list[1]`, or `self` itself allows are
//! left out, as no guard narrows those for it; with `strict` they count too, as TypeScript reads
//! them.
//!
//! Natives are checked as the runtime passes their arguments on: numbers and booleans pass for each
//! other, as a `BOOL` is an integer to them, a hash parameter hashes a string, and a vector fills
//! one number parameter for each of its parts. The native wrappers turn `nil`, `0` and `false` into
//! NULL for string parameters, and only for those, and other values into their text, so a string
//! parameter takes a number or a boolean written out, and the server id of a player, but not
//! another number, such as a hash. A value of unknown type that may be a vector leaves where the
//! values after it go unknown, when the call passes fewer values than the native takes. With
//! `strict`, as TypeScript reads the declared types, numbers and booleans no longer pass for each
//! other, and a string parameter takes no number or boolean, also one written out, while `nil`,
//! hashed strings and vectors still pass as the runtime takes them, and so does the number of a
//! player's server id, which only the native data calls a string.
//!
//! Parameters typed with a generic of the function called are left out, as the arguments of the
//! call bind them, unless the parameter is typed with the generic alone and another argument
//! declares it: `table.insert(list, value)` takes for `value` the type of the entries of a `list`
//! declared as `number[]`, as TypeScript checks `push<T>(list: T[], value: T)`. The function a
//! callee passes to a callback, such as `resolve` of
//! `fun(resolve: fun(value: T))`, takes and returns what the other arguments of that call declare
//! for its generics, as the `boolean` of `Promise:New('boolean', function(resolve) end)` for a
//! `` `T` ``; a generic they leave unbound takes any value.

use std::sync::Arc;

use qbx_lua_analysis::scope::{LocalKind, Resolved};
use qbx_lua_analysis::signature::{most_arguments, Requirement};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{SmolStr, Span};

use super::callback_payloads::Payload;
use super::class_tables::Classes;
use super::comparisons::Declared;
use super::unknown_types::is_typed;
use crate::index::{FileId, FileOrigin};
use crate::infer::{substitute, Infer, NATIVE_HANDLE_TYPES};
use crate::types::{FunType, Param, Type};

/// Each argument whose parameter does not take it, with the message naming both types, also among
/// the `payloads` of the wrapper calls of `chunk`.
pub fn mismatched_arguments(infer: &Infer, chunk: &Chunk, payloads: &[Payload]) -> Vec<(Span, String)> {
    checked_arguments(infer, chunk, payloads, None)
}

/// Each argument of no known type, of those `pick` picks, passed for a parameter whose type is
/// declared in each signature that the call may use, with the message naming the parameter.
pub fn unknown_arguments(
    infer: &Infer,
    chunk: &Chunk,
    payloads: &[Payload],
    pick: &dyn Fn(&Expr) -> bool,
) -> Vec<(Span, String)> {
    checked_arguments(infer, chunk, payloads, Some(pick))
}

fn checked_arguments(
    infer: &Infer,
    chunk: &Chunk,
    payloads: &[Payload],
    unknowns: Option<&dyn Fn(&Expr) -> bool>,
) -> Vec<(Span, String)> {
    let mut calls = Calls {
        infer,
        declared: Declared::new(infer),
        annotated: Declared::annotations(infer),
        classes: Classes::new(infer),
        unknowns,
        out: Vec::new(),
    };
    calls.visit_block(&chunk.block);
    for payload in payloads {
        let handlers: Vec<&FunType> = payload.handlers.iter().collect();
        calls.passed(&handlers, false, false, payload.args);
    }
    calls.out
}

struct Calls<'a, 'b> {
    infer: &'a Infer<'b>,
    declared: Declared<'a, 'b>,
    /// What only annotations declare, which decides the generics an argument is checked against: a
    /// list that a constructor builds declares nothing about the entries code adds to it.
    annotated: Declared<'a, 'b>,
    classes: Classes<'a, 'b>,
    /// When the arguments of no known type are looked for instead of mismatches, which of them
    /// count.
    unknowns: Option<&'a dyn Fn(&Expr) -> bool>,
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
        // A native's arguments are checked as the runtime converts them, which `no-unknown` leaves out.
        let native = method.is_none() && !args.is_empty() && self.declared.is_native(base, 0);
        let signatures = match native && self.unknowns.is_none() {
            true => definitions(self.infer, base, method),
            false => signatures(self.infer, &self.declared, call, base, method, args),
        };
        if signatures.is_empty() {
            return;
        }
        let signatures: Vec<&FunType> = signatures.iter().map(Arc::as_ref).collect();
        self.passed(&signatures, method.is_some(), native, args);
    }

    /// Checks `args`, passed to a function that has one of `signatures`, or to a `native`.
    fn passed(&mut self, signatures: &[&FunType], via_colon: bool, native: bool, args: &[Expr]) {
        if let Some(pick) = self.unknowns {
            return self.unknown(signatures, via_colon, args, pick);
        }
        let values: Vec<Type> = args.iter().map(|arg| self.value(arg)).collect();
        self.compare(signatures, via_colon, native, args, &values);
    }

    /// Reports each of `args` of no known type that `pick` picks, when the parameter it goes to has a
    /// declared type in each signature the call may use.
    fn unknown(&mut self, signatures: &[&FunType], via_colon: bool, args: &[Expr], pick: &dyn Fn(&Expr) -> bool) {
        let candidates = candidates(self.infer, signatures, via_colon, args);
        for (index, arg) in args.iter().enumerate() {
            let given = self.infer.cast_after(arg.span.end).unwrap_or_else(|| self.infer.expr(arg));
            if !given.is_unknown() || !pick(arg) {
                continue;
            }
            // The parameter it goes to in the first signature, when each has a declared type.
            let mut first: Option<&Param> = None;
            for fun in candidates.iter().copied() {
                let Some(param) = param_for(fun, via_colon, index) else {
                    first = None;
                    break;
                };
                let is_generic = |name: &str| fun.generics.iter().any(|generic| generic == name);
                let is_handle = |name: &str| NATIVE_HANDLE_TYPES.contains(&name);
                let expected = expected(param);
                if !is_typed(expected) || mentions(expected, &is_generic) || names(expected, &is_handle) {
                    first = None;
                    break;
                }
                first.get_or_insert(param);
            }
            if let Some(param) = first {
                let message = format!(
                    "The type of the value passed to parameter `{}` of type `{}` is unknown",
                    param.name,
                    expected(param)
                );
                self.out.push((arg.span, message));
            }
        }
    }

    /// The declared type of an argument, or the type a `--[[@as T]]` right after it gives it.
    fn value(&self, arg: &Expr) -> Type {
        if let Some(cast) = self.infer.cast_after(arg.span.end) {
            return cast;
        }
        let ty = self.declared.of(arg);
        match self.infer.strict() || !(reads_field(arg) || is_self(self.infer, arg)) {
            true => ty,
            false => self.infer.without_falsy(&ty),
        }
    }

    /// Reports the arguments that the signature closest to taking them does not take, unless one of
    /// them takes them all. Only the signatures that take as many arguments as the call passes count,
    /// unless none does.
    fn compare(&mut self, signatures: &[&FunType], via_colon: bool, native: bool, args: &[Expr], values: &[Type]) {
        let mut closest: Option<Vec<(Span, String)>> = None;
        for fun in candidates(self.infer, signatures, via_colon, args) {
            let found = self.mismatches(fun, via_colon, native, args, values);
            if found.is_empty() {
                return;
            }
            if closest.as_ref().is_none_or(|closest| found.len() < closest.len()) {
                closest = Some(found);
            }
        }
        self.out.extend(closest.unwrap_or_default());
    }

    /// Each of `args` that the parameter of `fun`, or of a `native`, it goes to does not take.
    fn mismatches(
        &self,
        fun: &FunType,
        via_colon: bool,
        native: bool,
        args: &[Expr],
        values: &[Type],
    ) -> Vec<(Span, String)> {
        let mut out = Vec::new();
        // The generics of `fun` that the annotated types of the arguments decide, as the `number` of
        // `table.insert(list, value)` for a list declared as `number[]`.
        let bound = match native || fun.generics.is_empty() {
            true => Vec::new(),
            false => {
                let annotated: Vec<Type> = args.iter().map(|arg| self.annotated.of(arg)).collect();
                self.infer.generics_bound_by(fun, args, via_colon, &annotated)
            }
        };
        // The parameters that the vectors passed before take beyond their first.
        let mut spread = 0;
        for (i, (arg, given)) in args.iter().zip(values).enumerate() {
            // The `self` a `.` call passes to a method has no parameter of its own.
            let Some(param) = param_for(fun, via_colon, i + spread) else { continue };
            if native && matches!(expected(param), Type::Number | Type::Integer) {
                match self.vector_parts(arg, given) {
                    Some(1) => {}
                    Some(parts) => {
                        spread += parts - 1;
                        continue;
                    }
                    None if args.len() - i < fun.params.len().saturating_sub(i + spread) => break,
                    None => {}
                }
            }
            let message = match (native, bound_param(fun, param, &bound)) {
                (true, _) => native_mismatch(&self.classes, param, arg, given, self.infer.strict()),
                (false, Some(param)) => mismatch(&self.classes, fun, &param, given),
                (false, None) => mismatch(&self.classes, fun, param, given),
            };
            out.extend(message.map(|message| (arg.span, message)));
        }
        out
    }

    /// How many parameters of a native `arg` fills, declared as `declared`: a vector fills one for
    /// each of its parts, as the runtime spreads it. Where nothing declares the type of `arg`, what
    /// it holds tells, as only its place is in question. `None` when that is not known, or the type
    /// allows vectors of different sizes, as the `vector` of the runtime stubs does.
    fn vector_parts(&self, arg: &Expr, declared: &Type) -> Option<usize> {
        let ty = match declared.is_unknown() {
            true => self.infer.cast_after(arg.span.end).unwrap_or_else(|| self.infer.expr(arg)),
            false => declared.clone(),
        };
        let parts = |ty: &Type| match ty {
            Type::Named(name, _) => match name.as_str() {
                "vector2" => Some(2),
                "vector3" => Some(3),
                "vector4" | "quat" => Some(4),
                _ => Some(1),
            },
            Type::Unknown | Type::Any => None,
            _ => Some(1),
        };
        match self.infer.expand_aliases(&ty.without_nil(), 0) {
            Type::Union(types) => {
                let mut sizes = types.iter().filter(|ty| **ty != Type::Nil).map(parts);
                let first = sizes.next()??;
                sizes.all(|size| size == Some(first)).then_some(first)
            }
            ty => parts(&ty),
        }
    }
}

/// Whether `expr` reads a field or a key, as `self.handle` and `list[1]` do. lua-language-server
/// leaves the `nil` and `false` of such a value out of its type checks, as it does not narrow it.
pub fn reads_field(expr: &Expr) -> bool {
    matches!(expr.unparen().kind, ExprKind::Field { safe: false, .. } | ExprKind::Index { safe: false, .. })
}

/// Whether `expr` is the `self` of a method defined with `:`.
fn is_self(infer: &Infer, expr: &Expr) -> bool {
    let ExprKind::Name(name) = &expr.unparen().kind else { return false };
    let Some(Resolved::Local(id)) = infer.ctx.resolution.resolve_at(name.span.start) else { return false };
    infer.ctx.resolution.local(id).kind == LocalKind::ImplicitSelf
}

/// The signatures that a call of `base`, or of its `method`, with `args` may use: those of each
/// function `definitions` finds, with their `@overload`s for the side of the call. None for a call
/// without arguments, for a native, whose arguments the runtime converts as `param-type-mismatch`
/// checks them on its own, and for
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

/// `param`, typed by a generic of `fun` alone, as the `value: T` of `table.insert`, with the type
/// that `bound` gives that generic, as the arguments of the call decide it: TypeScript checks an
/// argument against what the first argument that decides the generic gives it. A parameter that
/// only mentions a generic, as `list: T[]`, is what decides it, and stays unchecked. `None` when
/// `param` is no such parameter, or no argument decides the generic.
fn bound_param(fun: &FunType, param: &Param, bound: &[(SmolStr, Type)]) -> Option<Param> {
    let is_generic = |name: &str| fun.generics.iter().any(|generic| generic == name);
    let Type::Named(name, args) = expected(param).without_nil() else { return None };
    if !args.is_empty() || !is_generic(name.as_str()) {
        return None;
    }
    let ty = substitute(&param.ty, bound);
    let decided = !matches!(expected(&Param { ty: ty.clone(), ..Param::default() }), Type::Unknown | Type::Any);
    (decided && !mentions(&ty, &is_generic)).then(|| Param { ty, ..param.clone() })
}

/// The message for a value of type `given` that `param` of `fun` does not take.
pub fn mismatch(classes: &Classes, fun: &FunType, param: &Param, given: &Type) -> Option<String> {
    let expected = expected(param);
    let is_generic = |name: &str| fun.generics.iter().any(|generic| generic == name);
    // Resources declare classes such as `Vehicle` that share the name of a native handle, which a
    // parameter of that type may well mean.
    let is_handle = |name: &str| NATIVE_HANDLE_TYPES.contains(&name);
    if matches!(given, Type::Unknown | Type::Any) || mentions(expected, &is_generic) || names(expected, &is_handle) {
        return None;
    }
    let from = classes.file();
    if classes.admits_nil(given, from) && !takes_nil(classes, param) {
        return Some(format!("Cannot assign `{given}` to parameter `{}` of type `{expected}`", param.name));
    }
    rejected(classes, param, &given.without_nil())
}

/// The message for the value `arg` of type `given` that `param` of a native does not take, as the
/// runtime passes it on: numbers and booleans pass for each other, and a hash parameter takes the
/// hash of a string. The native wrappers turn `nil`, `0` and `false` into NULL for string
/// parameters and other values into their text, so a string parameter takes `nil`, a number or a
/// boolean written out, and the number of a player's server id; other numbers and booleans, such as
/// the hash that `GetHashKey` gives, are no string it is meant to get. Each type a union lists has
/// to pass.
fn native_mismatch(classes: &Classes, param: &Param, arg: &Expr, given: &Type, strict: bool) -> Option<String> {
    let expected = expected(param);
    if matches!(given, Type::Unknown | Type::Any) {
        return None;
    }
    let from = classes.file();
    if *expected != Type::String && classes.admits_nil(given, from) && !takes_nil(classes, param) {
        return Some(format!("Cannot assign `{given}` to parameter `{}` of type `{expected}`", param.name));
    }
    let given = given.without_nil();
    let player_id = PLAYER_ID_PARAMS.contains(&param.name.as_str());
    let written = is_literal(arg) || player_id;
    // With `strict`, as TypeScript reads a declared type, a number or a boolean is no string, and
    // a `BOOL` is no number. The server id of a player is a number to scripts either way: the
    // native data only declares it a string.
    let converts = |part: &Type| {
        let converted = |kind: Type| !classes.rejects(&kind, from, part);
        match expected {
            Type::String if player_id && strict => converted(Type::Number),
            Type::String => !strict && written && (converted(Type::Number) || converted(Type::Boolean)),
            Type::Handle(name) if name == "Hash" => converted(Type::String) || converted(Type::Boolean),
            Type::Number | Type::Integer | Type::Handle(_) => !strict && converted(Type::Boolean),
            Type::Boolean => !strict && converted(Type::Number),
            _ => false,
        }
    };
    let mut parts = Vec::new();
    classes.flatten(&given, from, &mut parts, 0);
    let rejected = |part: &&Type| **part != Type::Nil && !converts(part) && classes.rejects(expected, from, part);
    let part = parts.iter().find(rejected)?;
    let shown = classes.shown(expected, from, &given, part);
    Some(format!("Cannot assign `{shown}` to parameter `{}` of type `{expected}`", param.name))
}

/// The string parameters of natives that take the server id of a player, which scripts hold as a
/// number.
const PLAYER_ID_PARAMS: &[&str] = &["playerSrc", "eventTarget"];

/// Whether `expr` is a number or a boolean written out, as `0`, `-1` or `false`.
fn is_literal(expr: &Expr) -> bool {
    match &expr.unparen().kind {
        ExprKind::Number(_) | ExprKind::True | ExprKind::False => true,
        ExprKind::Unary { op: UnOp::Neg, expr } => matches!(expr.unparen().kind, ExprKind::Number(_)),
        _ => false,
    }
}

/// The message for a value of type `given`, with no `nil` in it, that `param` clearly does not
/// take: one that may be of another kind, or a literal it does not list.
fn rejected(classes: &Classes, param: &Param, given: &Type) -> Option<String> {
    let expected = expected(param);
    let from = classes.file();
    if matches!(given, Type::Unknown | Type::Nil) {
        return None;
    }
    let part = classes.rejected_part(expected, from, given)?;
    let shown = classes.shown(expected, from, given, &part);
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
                // A native is no global of the index.
                if globals.is_empty() {
                    return held();
                }
                let globals: Vec<_> = globals.into_iter().filter(|(file, _)| reaches(*file)).collect();
                if !globals.iter().all(|(file, _)| known(Some(*file))) {
                    return Vec::new();
                }
                let funs = globals.into_iter().filter_map(|(_, symbol)| symbol.ty.as_fun());
                funs.map(|fun| infer.with_global_extensions(fun, &name.text)).collect()
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
