use qbx_fivem_data::Side;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{Comment, SmolStr};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::env::argument_doc_anchor;
use crate::signature::{documented, Requirement};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arity {
    pub params: usize,
    /// How many arguments a call has to pass: all the parameters, or, when the doc comment of the
    /// function documents them, those up to the last one it requires.
    pub required: usize,
    pub vararg: bool,
}

impl Arity {
    pub fn of(func: &FuncBody) -> Self {
        let params = func.params.len();
        Self { params, required: params, vararg: func.vararg.is_some() }
    }

    /// The arity of `func` under the doc comment above `anchor`: a call has to pass the parameters
    /// up to the last one the comment requires, as `missing-parameter` counts them, so one marked
    /// optional, as `---@param model? string`, or typed to take `nil` may be left out. Without a
    /// comment that documents its parameters, it has to pass them all.
    pub fn documented(source: &str, comments: &[Comment], anchor: u32, func: &FuncBody) -> Self {
        let arity = Self::of(func);
        match documented(source, comments, anchor, func, false) {
            Some(fun) => {
                Self { required: Requirement::of(&fun, false, &|_| None).arguments.min(arity.params), ..arity }
            }
            None => arity,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EventRegistration {
    pub side: Option<Side>,
    pub handler: Option<Arity>,
    /// The resource whose script registers the handler, when known.
    pub resource: Option<SmolStr>,
}

/// Facts that one file needs to know about every other file: who handles which event and what
/// each resource exports. The linter fills it from the files it parsed, the language server from
/// its index.
#[derive(Clone, Debug, Default)]
pub struct CrossRefs {
    pub events: FxHashMap<SmolStr, Vec<EventRegistration>>,
    pub exports: FxHashMap<(SmolStr, SmolStr), Arity>,
    /// Resources whose Lua files were seen, so a missing export really is missing.
    pub resources: FxHashSet<SmolStr>,
    /// Resources with encrypted or unreadable scripts: what they export and handle beyond the
    /// readable part is unknown, so nothing may be called "missing" there.
    pub opaque_resources: FxHashSet<SmolStr>,
}

pub const EVENT_REGISTRATION_CALLS: &[&str] = &["RegisterNetEvent", "RegisterServerEvent", "AddEventHandler"];

impl CrossRefs {
    pub fn add_event(&mut self, name: SmolStr, side: Option<Side>, handler: Option<Arity>, resource: Option<SmolStr>) {
        self.events.entry(name).or_default().push(EventRegistration { side, handler, resource });
    }

    pub fn collect(&mut self, source: &str, chunk: &Chunk, side: Option<Side>, resource: Option<&str>) {
        if let Some(resource) = resource {
            self.resources.insert(SmolStr::new(resource));
        }
        let regions = crate::side_guard::SideRegions::of_chunk(chunk);
        let mut collector = Collector {
            refs: self,
            source,
            comments: &chunk.comments,
            side,
            regions,
            resource,
            functions: FxHashMap::default(),
            anchors: FxHashMap::default(),
        };
        collector.visit_block(&chunk.block);
    }
}

struct Collector<'a> {
    refs: &'a mut CrossRefs,
    source: &'a str,
    comments: &'a [Comment],
    side: Option<Side>,
    regions: crate::side_guard::SideRegions,
    resource: Option<&'a str>,
    functions: FxHashMap<SmolStr, Arity>,
    /// Where the statement starts whose value a call is, by where the call starts: the functions
    /// passed to it take the doc comment above that statement, as the language server reads it.
    anchors: FxHashMap<u32, u32>,
}

impl<'ast> Visitor<'ast> for Collector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let documented = |func: &FuncBody| Arity::documented(self.source, self.comments, stmt.span.start, func);
        match &stmt.kind {
            StmtKind::LocalFunction { name, func } => {
                self.functions.insert(name.text.clone(), documented(func));
            }
            StmtKind::Function { name, func } if name.path.is_empty() && name.method.is_none() => {
                self.functions.insert(name.base.text.clone(), documented(func));
            }
            StmtKind::Local { names, exprs, .. } => {
                for (name, expr) in names.iter().zip(exprs) {
                    if let ExprKind::Function(func) = &expr.kind {
                        self.functions.insert(name.name.text.clone(), documented(func));
                    }
                }
            }
            _ => {}
        }
        let values = match &stmt.kind {
            StmtKind::Local { exprs, .. } | StmtKind::Assign { exprs, .. } | StmtKind::Return(exprs) => &exprs[..],
            StmtKind::Expr(expr) => std::slice::from_ref(expr),
            _ => &[],
        };
        for value in values.iter().filter(|value| value.is_call()) {
            self.anchors.insert(value.span.start, stmt.span.start);
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let ExprKind::Call { callee, args, .. } = &expr.kind {
            let path = callee.dotted_path();
            let name = args.first().and_then(Expr::as_string);
            match (path.as_deref(), name) {
                (Some(call), Some(name)) if EVENT_REGISTRATION_CALLS.contains(&call) => {
                    let anchor = self.anchors.get(&expr.span.start).copied().unwrap_or(expr.span.start);
                    let handler = args.iter().skip(1).find_map(|arg| match &arg.kind {
                        ExprKind::Function(func) => {
                            let anchor = argument_doc_anchor(self.source, self.comments, arg.span.start, anchor);
                            Some(Arity::documented(self.source, self.comments, anchor, func))
                        }
                        _ => self.arity_of(arg),
                    });
                    let side = self.regions.effective(expr.span.start, self.side);
                    self.refs.add_event(name.clone(), side, handler, self.resource.map(SmolStr::new));
                }
                (Some("exports"), Some(name)) => {
                    if let (Some(resource), Some(arity)) = (self.resource, args.get(1).and_then(|a| self.arity_of(a))) {
                        self.refs.exports.insert((SmolStr::new(resource), name.clone()), arity);
                    } else if let Some(resource) = self.resource {
                        let unknown = Arity { params: 0, required: 0, vararg: true };
                        self.refs.exports.insert((SmolStr::new(resource), name.clone()), unknown);
                    }
                }
                // `exports(name, fn)` in a loop (ox_lib does this): the export names only exist at
                // runtime, so the literal ones are not the whole list.
                (Some("exports"), None) if !args.is_empty() => {
                    if let Some(resource) = self.resource {
                        self.refs.opaque_resources.insert(SmolStr::new(resource));
                    }
                }
                _ => {}
            }
        }
        visit::walk_expr(self, expr);
    }
}

impl Collector<'_> {
    fn arity_of(&self, expr: &Expr) -> Option<Arity> {
        match &expr.kind {
            ExprKind::Function(func) => Some(Arity::of(func)),
            ExprKind::Name(name) => self.functions.get(&name.text).copied(),
            _ => None,
        }
    }
}

/// Where a trigger call delivers its event and how many leading arguments are not payload.
pub fn trigger_target(call: &str, own_side: Option<Side>) -> Option<(Option<Side>, usize)> {
    Some(match call {
        "TriggerServerEvent" => (Some(Side::Server), 1),
        "TriggerLatentServerEvent" => (Some(Side::Server), 2),
        "TriggerClientEvent" => (Some(Side::Client), 2),
        "TriggerLatentClientEvent" => (Some(Side::Client), 3),
        "TriggerEvent" => (own_side.filter(|s| *s != Side::Shared), 1),
        _ => return None,
    })
}

impl CrossRefs {
    pub fn merge(&mut self, other: CrossRefs) {
        for (name, registrations) in other.events {
            self.events.entry(name).or_default().extend(registrations);
        }
        self.exports.extend(other.exports);
        self.resources.extend(other.resources);
        self.opaque_resources.extend(other.opaque_resources);
    }
}
