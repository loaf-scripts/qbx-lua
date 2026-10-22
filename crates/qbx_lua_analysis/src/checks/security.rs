use qbx_fivem_data::Side;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{SmolStr, Span};
use rustc_hash::FxHashSet;

use super::{FileInput, Sink};
use crate::rules;
use crate::scope::{LocalId, Resolved};

/// Calls whose arguments decide money, items, permissions, commands or code.
const SENSITIVE_METHODS: &[&str] = &[
    "AddMoney",
    "RemoveMoney",
    "SetMoney",
    "AddItem",
    "RemoveItem",
    "SetJob",
    "SetGang",
    "SetMetaData",
    "SetPlayerData",
    "addAccountMoney",
    "removeAccountMoney",
    "setAccountMoney",
    "addInventoryItem",
    "removeInventoryItem",
    "setJob",
];
const SENSITIVE_FUNCTIONS: &[&str] = &[
    "ExecuteCommand",
    "load",
    "loadstring",
    "os.execute",
    "io.open",
    "os.remove",
    "SaveResourceFile",
    "GiveWeaponToPed",
];
const VALIDATORS: &[&str] =
    &["assert", "type", "tonumber", "tostring", "math.type", "math.floor", "math.abs", "lib.assert"];
const PLAYER_ID_NAMES: &[&str] = &["source", "src", "playerSource", "playerSrc"];
/// Statements that change the schema. They take no `?` placeholders: what is built into them is
/// names and definitions of tables, columns and indexes, which no parameter can stand for.
const SCHEMA_STATEMENTS: &[&str] = &["ALTER", "CREATE", "DROP", "TRUNCATE", "RENAME"];

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    if input.side == Some(Side::Client) {
        return;
    }
    let mut net_events: FxHashSet<SmolStr> = FxHashSet::default();
    let mut checker = Security { input, sink, net_events: &mut net_events };
    checker.visit_block(&input.chunk.block);
}

struct Security<'a, 'b> {
    input: &'a FileInput<'a>,
    sink: &'a mut Sink<'b>,
    /// Events registered with `RegisterNetEvent('name')` and handled by a later `AddEventHandler`.
    net_events: &'a mut FxHashSet<SmolStr>,
}

impl Security<'_, '_> {
    fn sql(&mut self, path: &str, args: &[Expr]) {
        if !super::sql::is_database_call(path) {
            return;
        }
        let Some(query) = args.first() else { return };
        if changes_schema(query) {
            return;
        }
        // A parameter table next to the query means values are bound; what is concatenated then is
        // structure (a WHERE clause, a column list), which placeholders cannot express.
        let binds_parameters = args.get(1).is_some_and(|arg| !matches!(arg.kind, ExprKind::Function(_)));
        let built = match &query.unparen().kind {
            ExprKind::Binary { op: BinOp::Concat, .. } => !binds_parameters && has_dynamic_part(query),
            ExprKind::MethodCall { base, method, args, .. } if method.text == "format" && !args.is_empty() => {
                let only_identifiers = |text: &str| text.matches("%s").count() == text.matches("`%s`").count();
                base.unparen().as_string().is_some_and(|text| !only_identifiers(text))
            }
            _ => false,
        };
        if built {
            self.sink.report(
                rules::SQL_CONCATENATION,
                query.span,
                "this query is assembled from values; pass them as parameters with ? placeholders to rule out SQL injection",
            );
        }
    }

    fn net_handler(&mut self, func: &FuncBody) {
        // Modules without a known side may well be client code, where none of this applies.
        let is_server =
            self.input.side == Some(Side::Server) || (self.input.side.is_none() && self.input.resource.is_none());
        if !is_server {
            return;
        }
        let resolution = self.input.resolution;
        let params: Vec<(LocalId, &Name)> = func
            .params
            .iter()
            .filter_map(|p| match resolution.resolve_at(p.span.start)? {
                Resolved::Local(id) => Some((id, p)),
                Resolved::Global(_) => None,
            })
            .collect();
        if let Some((_, first)) = params.first().filter(|(_, p)| PLAYER_ID_NAMES.contains(&p.text.as_str())) {
            self.sink.report(
                rules::CLIENT_SUPPLIED_SOURCE,
                first.span,
                format!("'{}' is sent by the client and can be any player's id; use the global 'source' of the event instead", first.text),
            );
        }
        if params.is_empty() {
            return;
        }
        let mut scan = HandlerScan { guards: Vec::new(), sinks: Vec::new() };
        scan.visit_block(&func.body);
        for (id, name) in params {
            let local = resolution.local(id);
            let validated = local.refs.iter().any(|r| scan.guards.iter().any(|g| g.contains_span(r.span)));
            if validated {
                continue;
            }
            for (sink_name, arg_span) in
                scan.sinks.iter().filter(|(_, span)| local.refs.iter().any(|r| r.span == *span))
            {
                self.sink.report(
                    rules::UNVALIDATED_EVENT_ARGUMENT,
                    *arg_span,
                    format!("'{}' comes straight from the client and reaches {sink_name} without being checked anywhere in this handler", name.text),
                );
            }
        }
    }
}

/// Whether the query `query` builds starts with a statement that changes the schema, as
/// `('ALTER TABLE `%s` %s'):format(name, clauses)` does.
fn changes_schema(query: &Expr) -> bool {
    let mut start = query.unparen();
    loop {
        match &start.kind {
            ExprKind::Binary { op: BinOp::Concat, lhs, .. } => start = lhs.unparen(),
            ExprKind::MethodCall { base, method, .. } if method.text == "format" => start = base.unparen(),
            _ => break,
        }
    }
    let Some(text) = start.as_string() else { return false };
    let keyword = text.trim_start().split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or_default();
    SCHEMA_STATEMENTS.iter().any(|statement| keyword.eq_ignore_ascii_case(statement))
}

fn has_dynamic_part(expr: &Expr) -> bool {
    match &expr.unparen().kind {
        ExprKind::Binary { op: BinOp::Concat, lhs, rhs, .. } => has_dynamic_part(lhs) || has_dynamic_part(rhs),
        ExprKind::String(_) | ExprKind::Number(_) => false,
        _ => true,
    }
}

impl<'ast> Visitor<'ast> for Security<'_, '_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let ExprKind::Call { callee, args, .. } = &expr.kind {
            if let Some(path) = callee.dotted_path() {
                self.sql(&path, args);
                let name = args.first().and_then(Expr::as_string);
                let handler = args.iter().skip(1).find_map(|arg| match &arg.kind {
                    ExprKind::Function(func) => Some(func),
                    _ => None,
                });
                match (path.as_str(), name, handler) {
                    ("RegisterNetEvent" | "RegisterServerEvent", Some(name), None) => {
                        self.net_events.insert(name.clone());
                    }
                    ("RegisterNetEvent" | "RegisterServerEvent", _, Some(func)) => self.net_handler(func),
                    ("AddEventHandler", Some(name), Some(func)) if self.net_events.contains(name) => {
                        self.net_handler(func)
                    }
                    _ => {}
                }
            }
        }
        if let ExprKind::MethodCall { base, .. } = &expr.kind {
            if let Some(path) = base.dotted_path() {
                if let ExprKind::MethodCall { args, .. } = &expr.kind {
                    self.sql(&path, args);
                }
            }
        }
        visit::walk_expr(self, expr);
    }
}

struct HandlerScan {
    guards: Vec<Span>,
    sinks: Vec<(String, Span)>,
}

impl HandlerScan {
    fn sink_args(&mut self, label: String, args: &[Expr]) {
        for arg in args {
            if let ExprKind::Name(name) = &arg.unparen().kind {
                self.sinks.push((label.clone(), name.span));
            }
        }
    }
}

impl<'ast> Visitor<'ast> for HandlerScan {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::If { branches, .. } => self.guards.extend(branches.iter().map(|b| b.cond.span)),
            StmtKind::While { cond, .. } | StmtKind::Repeat { cond, .. } => self.guards.push(cond.span),
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Call { callee, args, .. } => {
                if let Some(path) = callee.dotted_path() {
                    let last = path.rsplit('.').next().unwrap_or(&path);
                    if VALIDATORS.contains(&path.as_str()) {
                        self.guards.push(expr.span);
                    } else if SENSITIVE_FUNCTIONS.contains(&path.as_str()) || SENSITIVE_METHODS.contains(&last) {
                        self.sink_args(path.clone(), args);
                    }
                }
            }
            ExprKind::MethodCall { method, args, .. } if SENSITIVE_METHODS.contains(&method.text.as_str()) => {
                self.sink_args(method.text.to_string(), args);
            }
            ExprKind::Binary { op: BinOp::And | BinOp::Or, lhs, .. } => self.guards.push(lhs.span),
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}
