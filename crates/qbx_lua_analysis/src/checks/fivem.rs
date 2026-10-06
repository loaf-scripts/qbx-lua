use qbx_fivem_data::{native, Side};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{NumberValue, SmolStr, Span};
use rustc_hash::FxHashSet;

use super::{FileInput, Sink};
use crate::diagnostic::{Fix, TextEdit};
use crate::env::builtins;
use crate::rules;
use crate::scope::Resolved;

const CITIZEN_ALIASES: &[&str] = &["Wait", "CreateThread", "SetTimeout", "ClearTimeout"];
pub(super) const DEFERRING_CALLS: &[&str] =
    &["SetTimeout", "CreateThread", "Citizen.SetTimeout", "Citizen.CreateThread", "Citizen.CreateThreadNow"];
const MAY_RUN_CALLBACKS: &[&str] =
    &["pcall", "xpcall", "load", "loadstring", "dofile", "require", "select", "assert", "error"];

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    let uses_ox_lib_cache =
        input.resource.is_some_and(|r| r.name != "ox_lib" && r.env.imports_path("@ox_lib/init.lua", Side::Client))
            && input.side != Some(Side::Server);
    let mut checker = FiveM {
        input,
        sink,
        functions: vec![FunctionState::default()],
        deferred: FxHashSet::default(),
        reported_inner: FxHashSet::default(),
        hash_literal_forbidden: FxHashSet::default(),
        uses_ox_lib_cache,
    };
    checker.visit_block(&input.chunk.block);
}

#[derive(Default)]
struct FunctionState {
    yielded_at: Option<Span>,
    deferred: bool,
}

struct FiveM<'a, 'b> {
    input: &'a FileInput<'a>,
    sink: &'a mut Sink<'b>,
    functions: Vec<FunctionState>,
    deferred: FxHashSet<u32>,
    reported_inner: FxHashSet<u32>,
    /// Calls used as statements or unparenthesized prefix expressions cannot become literals.
    hash_literal_forbidden: FxHashSet<Span>,
    uses_ox_lib_cache: bool,
}

fn is_yield_path(path: &str) -> bool {
    let last = path.rsplit('.').next().unwrap_or(path);
    matches!(path, "Wait" | "Citizen.Wait" | "Citizen.Await" | "coroutine.yield") || last.eq_ignore_ascii_case("await")
}

fn looks_yielding(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    is_yield_path(path) || ["wait", "yield", "sleep", "await"].iter().any(|word| lower.contains(word))
}

impl FiveM<'_, '_> {
    fn is_global(&self, name: &Name) -> bool {
        matches!(self.input.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
    }

    /// The dotted path of a callee whose root is an actual global, not a local that shadows one.
    fn global_path(&self, expr: &Expr) -> Option<String> {
        let mut root = expr;
        loop {
            match &root.kind {
                ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => root = base,
                ExprKind::Name(name) => return self.is_global(name).then(|| expr.dotted_path()).flatten(),
                _ => return None,
            }
        }
    }

    fn user_defined(&self, name: &str) -> bool {
        match &self.input.resource {
            Some(resource) => resource.env.defines(name, self.input.side),
            None => self.input.summary.global_defs.iter().any(|d| d.name == name),
        }
    }

    fn is_runtime_call(&self, path: &str) -> bool {
        let root = path.split('.').next().unwrap_or(path);
        if self.user_defined(root) || MAY_RUN_CALLBACKS.contains(&root) {
            return false;
        }
        native(root).is_some()
            || builtins().get(root).is_some_and(|_| !matches!(root, "exports" | "promise" | "coroutine"))
    }

    fn check_citizen_prefix(&mut self, expr: &Expr) {
        let ExprKind::Field { base, name, .. } = &expr.kind else { return };
        let ExprKind::Name(root) = &base.kind else { return };
        if root.text != "Citizen" || !self.is_global(root) || !CITIZEN_ALIASES.contains(&name.text.as_str()) {
            return;
        }
        if self.input.resolution.lookup_local_at(&name.text, expr.span.start).is_some() {
            return;
        }
        let fix = Fix {
            title: format!("Replace with '{}'", name.text),
            edits: vec![TextEdit { span: expr.span, new_text: name.text.to_string() }],
        };
        self.sink.report_with(
            rules::CITIZEN_PREFIX,
            expr.span,
            format!("use the global alias '{}' instead of 'Citizen.{}'", name.text, name.text),
            None,
            Some(fix),
        );
    }

    fn check_call(&mut self, expr: &Expr, callee: &Expr, args: &[Expr]) {
        let Some(path) = self.global_path(callee) else { return };
        if self.user_defined(path.split('.').next().unwrap_or(&path)) {
            return;
        }
        match (path.as_str(), args) {
            ("GetHashKey", [arg]) if !self.hash_literal_forbidden.contains(&expr.span) => {
                if let Some(value) = arg.as_string().filter(|v| !v.is_empty() && !v.contains(['`', '\n', '\r', '\\'])) {
                    let fix = Fix {
                        title: "Convert to a compile-time hash literal".into(),
                        edits: vec![TextEdit { span: expr.span, new_text: format!("`{value}`") }],
                    };
                    self.sink.report_with(
                        rules::HASH_LITERAL,
                        expr.span,
                        format!("use the compile-time hash literal `{value}` instead of calling GetHashKey at runtime"),
                        None,
                        Some(fix),
                    );
                }
            }
            ("GetPlayerPed", [arg]) if is_minus_one(arg) => {
                let fix = Fix {
                    title: "Replace with PlayerPedId()".into(),
                    edits: vec![TextEdit { span: expr.span, new_text: "PlayerPedId()".into() }],
                };
                self.sink.report_with(
                    rules::DEPRECATED_NATIVE_USAGE,
                    expr.span,
                    "use PlayerPedId() instead of GetPlayerPed(-1)",
                    None,
                    Some(fix),
                );
            }
            ("GetDistanceBetweenCoords" | "Vdist" | "Vdist2", _) => {
                self.sink.report(
                    rules::DEPRECATED_NATIVE_USAGE,
                    callee.span,
                    format!("'{path}' is a slow native call; subtract the vectors instead: #(coordsA - coordsB)"),
                );
            }
            _ => {}
        }
        if self.uses_ox_lib_cache {
            self.check_prefer_cache(expr, &path, args);
        }
    }

    fn check_prefer_cache(&mut self, expr: &Expr, path: &str, args: &[Expr]) {
        let inner_is = |arg: Option<&Expr>, name: &str| {
            arg.is_some_and(|a| matches!(&a.kind, ExprKind::Call { callee, args, .. } if args.is_empty() && callee.dotted_path().as_deref() == Some(name)))
        };
        let replacement = match path {
            "GetPlayerServerId" if inner_is(args.first(), "PlayerId") => "cache.serverId",
            "GetVehiclePedIsIn"
                if inner_is(args.first(), "PlayerPedId")
                    && args.get(1).is_some_and(|a| matches!(a.kind, ExprKind::False)) =>
            {
                "cache.vehicle"
            }
            "PlayerPedId" if args.is_empty() => "cache.ped",
            "PlayerId" if args.is_empty() => "cache.playerId",
            _ => return,
        };
        if self.reported_inner.contains(&expr.span.start) {
            return;
        }
        if let Some(inner) = args.first() {
            self.reported_inner.insert(inner.span.start);
        }
        self.sink.report(
            rules::PREFER_CACHE,
            expr.span,
            format!("ox_lib already tracks this value; use '{replacement}' instead of calling {path}()"),
        );
    }

    fn check_core_object(&mut self, expr: &Expr, base: &Expr, method: &Name) {
        if method.text != "GetCoreObject" {
            return;
        }
        let resource = match &base.kind {
            ExprKind::Field { base, name, .. } if base.dotted_path().as_deref() == Some("exports") => {
                Some(name.text.clone())
            }
            ExprKind::Index { base, index, .. } if base.dotted_path().as_deref() == Some("exports") => {
                index.as_string().cloned()
            }
            _ => None,
        };
        if resource.is_some_and(|r| r == "qb-core") {
            self.sink.report(
                rules::LEGACY_CORE_OBJECT,
                expr.span,
                "the QBCore object is a compatibility bridge; use exports.qbx_core and the qbx_core modules instead",
            );
        }
    }

    fn check_infinite_loop(&mut self, stmt: &Stmt, body: &Block) {
        let mut scan = LoopScan {
            checker: self,
            exits: false,
            may_yield: false,
            loop_depth: 0,
            labels: Vec::new(),
            gotos: Vec::new(),
        };
        scan.visit_block(body);
        let jumps_out = scan.gotos.iter().any(|target| !scan.labels.contains(target));
        if scan.exits || scan.may_yield || jumps_out {
            return;
        }
        let keyword = Span::new(stmt.span.start, stmt.span.start + 5.min(stmt.span.len()));
        self.sink.report(
            rules::LOOP_NEVER_YIELDS,
            keyword,
            "this loop never yields or exits; without a Wait() it freezes the thread it runs on",
        );
    }

    fn mark_deferred_callbacks(&mut self, callee: &Expr, args: &[Expr]) {
        let Some(path) = self.global_path(callee) else { return };
        let defers = DEFERRING_CALLS.contains(&path.as_str()) || path.starts_with("MySQL.");
        if !defers {
            return;
        }
        for arg in args {
            if let ExprKind::Function(func) = &arg.kind {
                self.deferred.insert(func.span.start);
            }
        }
    }

    fn check_source_read(&mut self, name: &Name) {
        if name.text != "source" || !self.is_global(name) || self.input.side == Some(Side::Client) {
            return;
        }
        let Some(state) = self.functions.last() else { return };
        let reason = if state.deferred {
            "inside a deferred callback"
        } else if state.yielded_at.is_some() {
            "after a yield"
        } else {
            return;
        };
        self.sink.report(
            rules::SOURCE_AFTER_YIELD,
            name.span,
            format!("global 'source' is read {reason}, when it may already belong to another event; copy it first: local src = source"),
        );
    }
}

fn is_minus_one(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Unary { op: UnOp::Neg, expr } if matches!(expr.kind, ExprKind::Number(NumberValue::Int(1))))
}

fn is_always_true(expr: &Expr) -> bool {
    matches!(expr.unparen().kind, ExprKind::True | ExprKind::Number(_))
}

impl<'ast> Visitor<'ast> for FiveM<'_, '_> {
    fn visit_func_body(&mut self, func: &'ast FuncBody) {
        let deferred = self.deferred.contains(&func.span.start);
        self.functions.push(FunctionState { yielded_at: None, deferred });
        visit::walk_func_body(self, func);
        self.functions.pop();
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::Expr(expr) => {
                self.hash_literal_forbidden.insert(expr.span);
            }
            StmtKind::While { cond, body } if is_always_true(cond) => self.check_infinite_loop(stmt, body),
            StmtKind::Repeat { body, cond } if matches!(cond.unparen().kind, ExprKind::False | ExprKind::Nil) => {
                self.check_infinite_loop(stmt, body)
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Call { callee: base, .. }
            | ExprKind::MethodCall { base, .. }
            | ExprKind::Field { base, .. }
            | ExprKind::Index { base, .. } => {
                if matches!(base.kind, ExprKind::Call { .. }) {
                    self.hash_literal_forbidden.insert(base.span);
                }
            }
            _ => {}
        }
        match &expr.kind {
            ExprKind::Name(name) => self.check_source_read(name),
            ExprKind::Field { .. } => self.check_citizen_prefix(expr),
            ExprKind::Call { callee, args, .. } => {
                self.mark_deferred_callbacks(callee, args);
                self.check_call(expr, callee, args);
                visit::walk_expr(self, expr);
                if callee.dotted_path().is_some_and(|p| is_yield_path(&p)) {
                    if let Some(state) = self.functions.last_mut() {
                        state.yielded_at.get_or_insert(expr.span);
                    }
                }
                return;
            }
            ExprKind::MethodCall { base, method, .. } => self.check_core_object(expr, base, method),
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

struct LoopScan<'c, 'a, 'b> {
    checker: &'c FiveM<'a, 'b>,
    exits: bool,
    may_yield: bool,
    loop_depth: u32,
    labels: Vec<SmolStr>,
    gotos: Vec<SmolStr>,
}

impl<'ast> Visitor<'ast> for LoopScan<'_, '_, '_> {
    fn visit_func_body(&mut self, _func: &'ast FuncBody) {}

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let is_loop = matches!(
            stmt.kind,
            StmtKind::While { .. }
                | StmtKind::Repeat { .. }
                | StmtKind::NumericFor { .. }
                | StmtKind::GenericFor { .. }
        );
        match &stmt.kind {
            StmtKind::Return(_) => self.exits = true,
            StmtKind::Break if self.loop_depth == 0 => self.exits = true,
            StmtKind::Goto(name) => self.gotos.push(name.text.clone()),
            StmtKind::Label(name) => self.labels.push(name.text.clone()),
            _ => {}
        }
        self.loop_depth += u32::from(is_loop);
        visit::walk_stmt(self, stmt);
        self.loop_depth -= u32::from(is_loop);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Call { callee, .. } => match callee.dotted_path() {
                Some(path) if looks_yielding(&path) => self.may_yield = true,
                Some(path) if self.checker.global_path(callee).is_some() && self.checker.is_runtime_call(&path) => {}
                _ => self.may_yield = true,
            },
            ExprKind::MethodCall { .. } => self.may_yield = true,
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}
