use qbx_fivem_data::{is_hash_native_name, native, Side, KNOWN_IMPORTS};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};

use rustc_hash::FxHashSet;

use super::fivem::DEFERRING_CALLS;
use super::{FileInput, Sink};
use crate::diagnostic::Tag;
use crate::env::{builtins, is_meta_file};
use crate::rules;
use crate::scope::{GlobalRef, GlobalRefKind, Resolution, Resolved, MAIN_CHUNK};
use crate::side_guard::{other, SideRegions};

/// Fields ox_lib adds to standard library tables when it is imported.
const OX_LIB_STD_EXTENSIONS: &[(&str, &str)] = &[
    ("table", "contains"),
    ("table", "matches"),
    ("table", "deepclone"),
    ("table", "merge"),
    ("table", "freeze"),
    ("table", "isfrozen"),
    ("table", "wipe"),
    ("table", "type"),
    ("string", "random"),
];

const AMBIGUOUS_IMPORT_GLOBALS: &[&str] = &["player", "_", "require"];

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    let regions = SideRegions::of(input.source, input.chunk);
    let shared = SharedCode::of(input);
    // A definition file declares the globals that other code defines, such as ox_lib's `noop` and
    // `cache`, which lua-language-server does not report as lowercase or implicit globals.
    let meta = is_meta_file(input.source, input.chunk);
    for global in &input.resolution.globals {
        match global.kind {
            GlobalRefKind::Read => check_read(input, global, &regions, &shared, sink),
            GlobalRefKind::Write | GlobalRefKind::FunctionDecl => check_definition(input, global, meta, sink),
        }
    }
    for field in input.resolution.env_fields.iter().filter(|field| field.nil_env) {
        sink.report(
            rules::GLOBAL_IN_NIL_ENV,
            field.span,
            format!("'{}' is looked up in the local _ENV, which is nil here, so this raises an error", field.name),
        );
    }
    Fields { input, sink }.visit_block(&input.chunk.block);
}

fn is_configured(input: &FileInput, name: &str) -> bool {
    input.config.globals.iter().any(|g| g == name)
}

fn defined_in_project(input: &FileInput, name: &str) -> bool {
    match &input.resource {
        Some(resource) => resource.env.defines(name, input.side),
        None => input.summary.global_defs.iter().any(|d| d.name == name),
    }
}

/// The code of a shared script, which runs on both sides, as far as names that only one side has go.
struct SharedCode {
    /// The functions that run on every side the script loads on, as long as the side is not known.
    load_time: Option<FxHashSet<u32>>,
}

impl SharedCode {
    fn of(input: &FileInput) -> Self {
        let shared = input.side == Some(Side::Shared) && !input.config.strict();
        Self { load_time: shared.then(|| load_time_functions(input.resolution, input.chunk)) }
    }

    /// Whether the code at `global`, in an unguarded part of a shared script, runs on both sides.
    /// With `strict`, as TypeScript knows no sides, any code may. Otherwise only code that runs
    /// while the script loads does: outside function bodies, and in the functions that
    /// `CreateThread` or `SetTimeout` start then. A function the script defines is left to the side
    /// that calls it, as the callbacks of a shared config only one side calls are.
    fn runs_on_both(&self, input: &FileInput, global: &GlobalRef) -> bool {
        let Some(load_time) = &self.load_time else { return true };
        let function = input.resolution.functions.get(global.func as usize);
        global.func == MAIN_CHUNK || function.is_some_and(|function| load_time.contains(&function.span.start))
    }
}

/// The functions, by where they start, that `CreateThread` or `SetTimeout` start while the script
/// loads, also from such a function.
fn load_time_functions(resolution: &Resolution, chunk: &Chunk) -> FxHashSet<u32> {
    struct LoadTime<'a> {
        resolution: &'a Resolution,
        at_load: bool,
        functions: FxHashSet<u32>,
    }

    impl<'ast> Visitor<'ast> for LoadTime<'_> {
        fn visit_func_body(&mut self, func: &'ast FuncBody) {
            let outer = std::mem::replace(&mut self.at_load, self.functions.contains(&func.span.start));
            visit::walk_func_body(self, func);
            self.at_load = outer;
        }

        fn visit_expr(&mut self, expr: &'ast Expr) {
            if let (true, ExprKind::Call { callee, args, .. }) = (self.at_load, &expr.kind) {
                if starts_thread(self.resolution, callee) {
                    let started = args.iter().filter_map(|arg| match &arg.kind {
                        ExprKind::Function(func) => Some(func.span.start),
                        _ => None,
                    });
                    self.functions.extend(started);
                }
            }
            visit::walk_expr(self, expr);
        }
    }

    let mut finder = LoadTime { resolution, at_load: true, functions: FxHashSet::default() };
    finder.visit_block(&chunk.block);
    finder.functions
}

/// Whether `callee` is the global `CreateThread`, `SetTimeout` or one of their `Citizen` names.
fn starts_thread(resolution: &Resolution, callee: &Expr) -> bool {
    let mut root = callee;
    while let ExprKind::Field { base, .. } = &root.kind {
        root = base;
    }
    let ExprKind::Name(name) = &root.kind else { return false };
    matches!(resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
        && callee.dotted_path().is_some_and(|path| DEFERRING_CALLS.contains(&path.as_str()))
}

/// `fivem/native-wrong-side` for a global the resource defines in the scripts of one side only:
/// read from code of a shared script that also runs on the other side, or that a guard keeps there.
fn check_one_sided(input: &FileInput, global: &GlobalRef, guarded: Option<Side>, shared: &SharedCode, sink: &mut Sink) {
    let (Some(resource), Some(Side::Shared)) = (&input.resource, input.side) else { return };
    let name = global.name.as_str();
    let only = match (resource.env.defines(name, Some(Side::Client)), resource.env.defines(name, Some(Side::Server))) {
        (true, false) => Side::Client,
        (false, true) => Side::Server,
        _ => return,
    };
    let place = match guarded {
        Some(side) if side != only => format!("code that only runs on the {}", side.label()),
        None if shared.runs_on_both(input, global) => {
            format!("a shared script that also runs on the {}", other(only).label())
        }
        _ => return,
    };
    let message = format!("'{name}' is only defined by {} scripts of this resource, but this is {place}", only.label());
    sink.report(rules::NATIVE_WRONG_SIDE, global.span, message);
}

fn check_read(input: &FileInput, global: &GlobalRef, regions: &SideRegions, shared: &SharedCode, sink: &mut Sink) {
    let name = global.name.as_str();
    if is_configured(input, name) {
        return;
    }
    let guarded = regions.side_at(global.span.start);
    if defined_in_project(input, name) {
        check_one_sided(input, global, guarded, shared, sink);
        return;
    }
    let side = guarded.or(input.side).unwrap_or(Side::Shared);
    let place = if guarded.is_some() { "code that only runs on the" } else { "a" };
    let unit = if guarded.is_some() { "" } else { " script" };
    // A shared script also runs on the side that lacks a name, unless a guard keeps the code off it.
    let unguarded_shared = guarded.is_none() && input.side == Some(Side::Shared);
    if let Some(builtin) = builtins().get(name) {
        // A standard library is missing anywhere on that side; other CfxLua globals, like natives,
        // only where the code runs on both sides.
        let missing_from_shared =
            builtin.side != Side::Shared && unguarded_shared && (builtin.library || shared.runs_on_both(input, global));
        if !builtin.side.is_available_on(side) {
            sink.report(
                rules::NATIVE_WRONG_SIDE,
                global.span,
                format!(
                    "'{name}' only exists on the {}, but this is {place} {}{unit}",
                    builtin.side.label(),
                    side.label()
                ),
            );
        } else if missing_from_shared {
            sink.report(
                rules::NATIVE_WRONG_SIDE,
                global.span,
                format!(
                    "'{name}' only exists on the {}, but this shared script also runs on the {}",
                    builtin.side.label(),
                    other(builtin.side).label()
                ),
            );
        } else if builtin.deprecated {
            sink.report_with(
                rules::DEPRECATED,
                global.span,
                format!("'{name}' is deprecated"),
                Some(Tag::Deprecated),
                None,
            );
        }
        return;
    }
    if let Some(native) = native(name) {
        if !native.side.is_available_on(side) {
            sink.report(
                rules::NATIVE_WRONG_SIDE,
                global.span,
                format!("native '{name}' is {}-only, but this is {place} {}{unit}", native.side.label(), side.label()),
            );
        } else if native.side != Side::Shared && unguarded_shared && shared.runs_on_both(input, global) {
            sink.report(
                rules::NATIVE_WRONG_SIDE,
                global.span,
                format!(
                    "native '{name}' is {}-only, but this shared script also runs on the {}",
                    native.side.label(),
                    other(native.side).label()
                ),
            );
        }
        return;
    }
    if is_hash_native_name(name) {
        return;
    }

    if let Some(resource) = &input.resource {
        if resource.env.opaque {
            return;
        }
        let provider = KNOWN_IMPORTS
            .iter()
            .filter(|_| !AMBIGUOUS_IMPORT_GLOBALS.contains(&name))
            .find(|import| import.globals.contains(&name));
        if let Some(provider) = provider {
            if resource.env.loads_module(provider.path) {
                return;
            }
            let scripts = input.side.map_or("this resource's", |s| s.label());
            sink.report(
                rules::IMPORT_NOT_DECLARED,
                global.span,
                format!(
                    "'{name}' comes from '{}', which fxmanifest.lua does not load for {scripts} scripts",
                    provider.path
                ),
            );
            return;
        }
        if resource.env.declares(name, input.side) {
            return;
        }
        if let Some(import) = resource.env.has_unresolved_import_for(input.side) {
            sink.report(
                rules::UNDEFINED_GLOBAL,
                global.span,
                format!(
                    "undefined global '{name}' (it may come from '{}', which could not be found; declare it under 'globals' in qbxlint.toml if so)",
                    import.path
                ),
            );
            return;
        }
    }
    sink.report(rules::UNDEFINED_GLOBAL, global.span, format!("undefined global '{name}'"));
}

fn check_definition(input: &FileInput, global: &GlobalRef, meta: bool, sink: &mut Sink) {
    let name = global.name.as_str();
    if is_configured(input, name) {
        return;
    }
    let is_runtime_name = builtins().get(name).is_some() || native(name).is_some();
    if is_runtime_name {
        let what = if native(name).is_some() { "native" } else { "runtime global" };
        sink.report(rules::BUILTIN_OVERWRITE, global.span, format!("overwriting {what} '{name}'"));
        return;
    }
    if meta {
        return;
    }
    if global.func == MAIN_CHUNK {
        if name.starts_with(|c: char| c.is_ascii_lowercase()) {
            sink.report(
                rules::LOWERCASE_GLOBAL,
                global.span,
                format!("global '{name}' starts with a lowercase letter; did you forget 'local'?"),
            );
        }
        return;
    }
    // An encrypted script of an opaque resource may declare it at file scope.
    let declared = match &input.resource {
        Some(resource) => resource.env.opaque || resource.env.declared_at_file_scope(name),
        None => input.summary.global_defs.iter().any(|d| d.at_file_scope && d.name == name),
    };
    if !declared {
        sink.report(
            rules::IMPLICIT_GLOBAL,
            global.span,
            format!(
                "'{name}' becomes a global here and is not declared at file scope anywhere; did you forget 'local'?"
            ),
        );
    }
}

struct Fields<'a, 'b> {
    input: &'a FileInput<'a>,
    sink: &'a mut Sink<'b>,
}

impl Fields<'_, '_> {
    fn check_field(&mut self, base: &Name, field: &Name) {
        if field.is_missing() || !matches!(self.input.resolution.resolve_at(base.span.start), Some(Resolved::Global(_)))
        {
            return;
        }
        let table = base.text.as_str();
        let Some(builtin) = builtins().closed_table(table) else { return };
        if builtin.deprecated_fields.contains(&field.text) {
            let message = format!("'{table}.{}' is deprecated", field.text);
            self.sink.report_with(rules::DEPRECATED, field.span, message, Some(Tag::Deprecated), None);
            return;
        }
        if builtin.fields.contains(&field.text) || defined_in_project(self.input, table) {
            return;
        }
        let defined_here = self.input.summary.global_field_defs.iter().any(|(t, f)| t == table && *f == field.text);
        let resource_defines = self.input.resource.is_some_and(|r| {
            r.env.defines_field(table, &field.text)
                || (OX_LIB_STD_EXTENSIONS.contains(&(table, field.text.as_str()))
                    && r.env.imports_path("@ox_lib/init.lua", self.input.side.unwrap_or(Side::Shared)))
        });
        if !defined_here && !resource_defines {
            self.sink.report(rules::UNDEFINED_FIELD, field.span, format!("'{table}' has no field '{}'", field.text));
        }
    }
}

impl<'ast> Visitor<'ast> for Fields<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let StmtKind::Assign { targets, exprs } = &stmt.kind {
            for target in targets {
                match &target.kind {
                    ExprKind::Field { base, .. } => self.visit_expr(base),
                    _ => self.visit_expr(target),
                }
            }
            exprs.iter().for_each(|e| self.visit_expr(e));
            return;
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let ExprKind::Field { base, name, .. } = &expr.kind {
            if let ExprKind::Name(base) = &base.kind {
                self.check_field(base, name);
            }
        }
        visit::walk_expr(self, expr);
    }
}
