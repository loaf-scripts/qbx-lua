use qbx_fivem_data::Side;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::SmolStr;
use rustc_hash::FxHashSet;

use super::{FileInput, Sink};
use crate::crossref::{trigger_target, Arity, CrossRefs, EventRegistration};
use crate::project::side_of;
use crate::rules;
use crate::scope::Resolved;
use crate::side_guard::SideRegions;

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    let mentions_resource_state = input.source.contains("GetResourceState");
    let mut checker = CrossFile {
        input,
        sink,
        reported_dependencies: FxHashSet::default(),
        mentions_resource_state,
        selected_at_runtime: u32::from(is_bridge_file(input)),
        conditional: 0,
        regions: SideRegions::of(input.source, input.chunk),
    };
    checker.visit_block(&input.chunk.block);
}

const BRIDGE_FOLDERS: &[&str] = &["bridge", "bridges", "framework", "frameworks", "compat", "integrations"];

/// Per-framework files such as `bridge/esx/server.lua` are picked by a loader at runtime: either
/// the manifest does not run them as scripts at all, or they sit in a folder named for the purpose.
fn is_bridge_file(input: &FileInput) -> bool {
    // A side from the configuration does not make the manifest run the file.
    let loaded_on_demand = input.resource.is_some_and(|r| side_of(r.manifest, input.relative_path).is_none());
    let in_bridge_folder = input
        .relative_path
        .split('/')
        .rev()
        .skip(1)
        .any(|folder| BRIDGE_FOLDERS.contains(&folder.to_ascii_lowercase().as_str()));
    loaded_on_demand || in_bridge_folder
}

struct CrossFile<'a, 'b> {
    input: &'a FileInput<'a>,
    sink: &'a mut Sink<'b>,
    reported_dependencies: FxHashSet<SmolStr>,
    mentions_resource_state: bool,
    /// Depth of enclosing code that only runs when a string-valued setting selects it.
    selected_at_runtime: u32,
    /// Depth of enclosing `if` branches of any kind.
    conditional: u32,
    regions: SideRegions,
}

pub(super) fn passed_count(args: &[Expr], skip: usize) -> Option<usize> {
    let payload = args.get(skip..)?;
    match payload.last() {
        Some(last) if last.is_multi_value() => None,
        _ => Some(payload.len()),
    }
}

pub(super) fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// Events of the artifact's system resources: their handlers live outside any resources folder,
/// so nothing can be said about the side or arguments they expect.
fn is_system_event(name: &str) -> bool {
    let owner = name.split(':').next().unwrap_or(name);
    crate::startup::SYSTEM_RESOURCES.contains(&owner) || name == "chatMessage"
}

impl CrossFile<'_, '_> {
    fn is_global(&self, name: &Name) -> bool {
        matches!(self.input.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
    }

    fn trigger(&mut self, refs: &CrossRefs, call: &str, expr: &Expr, args: &[Expr]) {
        let own_side = self.regions.effective(expr.span.start, self.input.side);
        let Some((target, skip)) = trigger_target(call, own_side) else { return };
        let Some(name_arg) = args.first() else { return };
        let Some(name) = name_arg.as_string() else { return };
        let Some(registrations) = refs.events.get(name) else { return };

        let reaches = |side: Option<Side>| match (target, side) {
            (Some(target), Some(side)) => side.is_available_on(target),
            _ => true,
        };
        let own = self.input.resource.map(|r| r.name);
        let owned_by = |registration: &EventRegistration| own.is_some() && registration.resource.as_deref() == own;
        let reachable: Vec<_> = registrations.iter().filter(|r| reaches(r.side)).collect();
        if reachable.is_empty() {
            // Events are named `resource:event` by convention; an escrowed resource may well handle
            // this one on the other side too, inside a file that cannot be read.
            let owner = name.split(':').next().unwrap_or(name);
            if refs.opaque_resources.contains(owner) || is_system_event(name) {
                return;
            }
            // A local event that only other resources handle is a hook offered to them: on this
            // side it is theirs to listen for. The same goes for a local copy of an event the file
            // also sends across the network, the usual way to notify listeners on both sides.
            if call == "TriggerEvent"
                && ((own.is_some() && !registrations.iter().any(&owned_by)) || self.mirrors_over_network(name))
            {
                return;
            }
            let (Some(target), Some(other)) = (target, registrations.iter().find_map(|r| r.side)) else { return };
            self.sink.report(
                rules::EVENT_WRONG_SIDE,
                name_arg.span,
                format!(
                    "'{name}' is only handled on the {}, but {call} delivers it to the {}",
                    other.label(),
                    target.label()
                ),
            );
            return;
        }

        let Some(passed) = passed_count(args, skip) else { return };
        let arities: Vec<Arity> = reachable.iter().filter_map(|r| r.handler).collect();
        if arities.is_empty() || arities.iter().any(|a| a.vararg) {
            return;
        }
        // Another resource's handler may ignore trailing payload on purpose, so only the
        // resource's own handlers decide whether a trigger passes too much.
        let most = if own.is_some() {
            reachable.iter().filter(|r| owned_by(r)).filter_map(|r| r.handler).map(|a| a.params).max()
        } else {
            arities.iter().map(|a| a.params).max()
        };
        let least = arities.iter().map(|a| a.params).min().unwrap_or(0);
        if most.is_some_and(|most| passed > most) {
            let most = most.unwrap_or(0);
            self.sink.report(
                rules::EVENT_ARGUMENT_COUNT,
                expr.span,
                format!(
                    "'{name}' is triggered with {passed} argument{}, but its handler only takes {most}",
                    plural(passed)
                ),
            );
        } else if passed < least {
            self.sink.report(
                rules::EVENT_MISSING_ARGUMENTS,
                expr.span,
                format!(
                    "'{name}' is triggered with {passed} argument{}, but its handler declares {least}; the rest will be nil",
                    plural(passed)
                ),
            );
        }
    }

    /// `AddEventHandler('playerDropped', ...)` in client code: FiveM triggers that event on the
    /// server only, so the handler never runs.
    fn builtin_handler(&mut self, call: &str, expr: &Expr, args: &[Expr]) {
        if !matches!(call, "AddEventHandler" | "RegisterNetEvent" | "RegisterServerEvent") {
            return;
        }
        let Some(name_arg) = args.first() else { return };
        let Some(name) = name_arg.as_string() else { return };
        let Some(event) = qbx_fivem_data::builtin_event(name) else { return };
        let Some(own_side) = self.regions.effective(expr.span.start, self.input.side) else { return };
        if event.side.is_available_on(own_side) {
            return;
        }
        self.sink.report(
            rules::EVENT_WRONG_SIDE,
            name_arg.span,
            format!("'{name}' is a {} event; this {} handler never runs", event.side.label(), own_side.label()),
        );
    }

    /// Whether this file also delivers the event to the other side with a network trigger.
    fn mirrors_over_network(&self, event: &str) -> bool {
        ["TriggerServerEvent", "TriggerClientEvent", "TriggerLatentServerEvent", "TriggerLatentClientEvent"].iter().any(
            |call| ['\'', '"'].iter().any(|quote| self.input.source.contains(&format!("{call}({quote}{event}{quote}"))),
        )
    }

    fn exported_resource<'e>(&self, base: &'e Expr) -> Option<(SmolStr, &'e Expr)> {
        let (root, resource) = match &base.kind {
            ExprKind::Field { base, name, .. } => (base, name.text.clone()),
            ExprKind::Index { base, index, .. } => (base, index.as_string()?.clone()),
            _ => return None,
        };
        match &root.kind {
            ExprKind::Name(name) if name.text == "exports" && self.is_global(name) => Some((resource, base)),
            _ => None,
        }
    }

    fn export_call(&mut self, expr: &Expr, base: &Expr, method: &Name, args: &[Expr]) {
        let Some((resource, resource_expr)) = self.exported_resource(base) else { return };
        let own = self.input.resource.map(|r| r.name);
        if own == Some(resource.as_str()) {
            return;
        }
        self.dependency(&resource, resource_expr);

        let Some(refs) = self.input.crossrefs else { return };
        if !refs.resources.contains(&resource) {
            return;
        }
        match refs.exports.get(&(resource.clone(), method.text.clone())) {
            Some(arity) if !arity.vararg => {
                if let Some(passed) = passed_count(args, 0).filter(|passed| *passed > arity.params) {
                    self.sink.report(
                        rules::EXPORT_ARGUMENT_COUNT,
                        expr.span,
                        format!(
                            "export '{}' of '{resource}' takes {} argument{}, but {passed} are passed",
                            method.text,
                            arity.params,
                            plural(arity.params)
                        ),
                    );
                }
            }
            Some(_) => {}
            None => {
                let has_any = refs.exports.keys().any(|(r, _)| *r == resource);
                let could_be_hidden = refs.opaque_resources.contains(&resource);
                if has_any && !could_be_hidden && !method.is_missing() {
                    self.sink.report(
                        rules::UNKNOWN_EXPORT,
                        method.span,
                        format!("'{resource}' does not register an export named '{}'", method.text),
                    );
                }
            }
        }
    }

    fn dependency(&mut self, resource: &SmolStr, at: &Expr) {
        let Some(own) = self.input.resource else { return };
        let listed = own.manifest.dependencies.iter().any(|d| d.value.trim_start_matches('/') == resource.as_str());
        let imported = own.manifest.imports().any(|s| s.pattern.starts_with(&format!("@{resource}/")));
        // `if GetResourceState('x') == 'started'` marks an optional integration, not a hard dependency.
        let guarded = self.mentions_resource_state
            && self
                .input
                .source
                .split("GetResourceState")
                .skip(1)
                .any(|rest| rest.get(..resource.len() + 6).unwrap_or(rest).contains(resource.as_str()));
        // Bridge code picks one of many integrations at runtime (`if Config.Inventory == 'ox' then`);
        // none of them is a requirement, and most are not even installed.
        if listed || imported || guarded || self.selected_at_runtime > 0 {
            return;
        }
        if own.installed.is_some_and(|installed| !installed.contains(resource)) {
            // Inside any `if` the author may well be checking a flag first, so only a call that
            // always runs is worth a warning. A missing resource needs no dependency hint either.
            if self.conditional == 0 && self.reported_dependencies.insert(resource.clone()) {
                self.sink.report(
                    rules::RESOURCE_NOT_FOUND,
                    at.span,
                    format!("'{resource}' is called unconditionally, but no resource with that name is installed on this server"),
                );
            }
            return;
        }
        if !self.reported_dependencies.insert(resource.clone()) {
            return;
        }
        if own.started_before.is_some_and(|before| before.contains(resource)) {
            return;
        }
        let cfg_note = if own.started_before.is_some() { " and server.cfg does not start it earlier" } else { "" };
        self.sink.report(
            rules::MANIFEST_MISSING_DEPENDENCY,
            at.span,
            format!("'{resource}' is used but not listed under dependencies in fxmanifest.lua{cfg_note}, so it may start after this resource"),
        );
    }
}

/// `Config.Inventory == 'ox'`, `framework ~= "qbx"`: a condition that selects an integration by name.
fn compares_with_string(cond: &Expr) -> bool {
    match &cond.unparen().kind {
        ExprKind::Binary { op: BinOp::Eq | BinOp::Ne, lhs, rhs, .. } => {
            lhs.unparen().as_string().is_some() || rhs.unparen().as_string().is_some()
        }
        ExprKind::Binary { op: BinOp::And | BinOp::Or, lhs, rhs, .. } => {
            compares_with_string(lhs) || compares_with_string(rhs)
        }
        ExprKind::Unary { op: UnOp::Not, expr } => compares_with_string(expr),
        _ => false,
    }
}

/// `if Config.Framework ~= 'qbx' then return end` at the top of a bridge file.
fn is_selector_guard(stmt: &Stmt) -> bool {
    match &stmt.kind {
        StmtKind::If { branches, else_block: None } => match branches.as_slice() {
            [only] => {
                compares_with_string(&only.cond)
                    && matches!(only.block.stmts.last(), Some(Stmt { kind: StmtKind::Return(_), .. }))
            }
            _ => false,
        },
        _ => false,
    }
}

impl<'ast> Visitor<'ast> for CrossFile<'_, '_> {
    fn visit_block(&mut self, block: &'ast Block) {
        let mut guards = 0;
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
            if is_selector_guard(stmt) {
                guards += 1;
                self.selected_at_runtime += 1;
            }
        }
        self.selected_at_runtime -= guards;
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let StmtKind::If { branches, else_block } = &stmt.kind else {
            return visit::walk_stmt(self, stmt);
        };
        let selects = branches.iter().any(|b| compares_with_string(&b.cond));
        for branch in branches {
            self.visit_expr(&branch.cond);
        }
        self.selected_at_runtime += u32::from(selects);
        self.conditional += 1;
        for branch in branches {
            self.visit_block(&branch.block);
        }
        if let Some(block) = else_block {
            self.visit_block(block);
        }
        self.conditional -= 1;
        self.selected_at_runtime -= u32::from(selects);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Call { callee, args, .. } => {
                if let ExprKind::Name(name) = &callee.kind {
                    if self.is_global(name) {
                        self.builtin_handler(&name.text, expr, args);
                        if let Some(refs) = self.input.crossrefs {
                            self.trigger(refs, &name.text, expr, args);
                        }
                    }
                }
                // `pcall(function() return exports.x:Get() end)` probes for an optional resource.
                if matches!(callee.dotted_path().as_deref(), Some("pcall" | "xpcall")) {
                    self.selected_at_runtime += 1;
                    visit::walk_expr(self, expr);
                    self.selected_at_runtime -= 1;
                    return;
                }
            }
            ExprKind::MethodCall { base, method, args, .. } => self.export_call(expr, base, method, args),
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}
