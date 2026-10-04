use qbx_fivem_data::{is_hash_native_name, native, Side, KNOWN_IMPORTS};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};

use super::{FileInput, Sink};
use crate::diagnostic::Tag;
use crate::env::builtins;
use crate::rules;
use crate::scope::{GlobalRef, GlobalRefKind, Resolved, MAIN_CHUNK};
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
    for global in &input.resolution.globals {
        match global.kind {
            GlobalRefKind::Read => check_read(input, global, &regions, sink),
            GlobalRefKind::Write | GlobalRefKind::FunctionDecl => check_definition(input, global, sink),
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

fn check_read(input: &FileInput, global: &GlobalRef, regions: &SideRegions, sink: &mut Sink) {
    let name = global.name.as_str();
    if defined_in_project(input, name) || is_configured(input, name) {
        return;
    }
    let guarded = regions.side_at(global.span.start);
    let side = guarded.or(input.side).unwrap_or(Side::Shared);
    let place = if guarded.is_some() { "code that only runs on the" } else { "a" };
    let unit = if guarded.is_some() { "" } else { " script" };
    if let Some(builtin) = builtins().get(name) {
        // A shared script also runs where a standard library is missing, unless a guard keeps the
        // code off that side. Natives and CfxLua globals are left to the side calling the code.
        let missing_from_shared =
            builtin.library && builtin.side != Side::Shared && guarded.is_none() && input.side == Some(Side::Shared);
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

fn check_definition(input: &FileInput, global: &GlobalRef, sink: &mut Sink) {
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
    let declared = match &input.resource {
        Some(resource) => resource.env.declared_at_file_scope(name),
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
