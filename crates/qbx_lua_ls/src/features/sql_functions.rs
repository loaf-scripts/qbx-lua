//! The functions that take an SQL query, for editors that highlight the query in their calls: those
//! whose first parameter has the `sql` type, which the runtime stubs declare as a `string`.
use qbx_luacats::types::{FunType, Type};

use crate::index::{FileOrigin, Index, SymbolKind};

/// The functions of the workspace and its libraries whose first parameter takes `sql`, as calls
/// write them: `Fetch`, `DB.fetch`, `DB:fetch` for a function defined with `:`, or
/// `exports.resource:fetch` for an export. Sorted, each once.
pub fn sql_functions(index: &Index) -> Vec<String> {
    let mut out = Vec::new();
    for (_, file) in index.files().filter(|(_, file)| file.origin != FileOrigin::Stub) {
        for global in &file.index.globals {
            if let Type::Fun(fun) = &global.ty {
                if takes_sql(fun) {
                    out.push(global.name.to_string());
                }
            }
        }
        for member in &file.index.members {
            let Type::Fun(fun) = &member.symbol.ty else { continue };
            if takes_sql(fun) {
                let separator = if member.symbol.kind == SymbolKind::Method { ':' } else { '.' };
                out.push(format!("{}{separator}{}", member.owner, member.symbol.name));
            }
        }
        let resource = file.resource.and_then(|id| index.resource(id));
        for export in &file.index.exports {
            if let (Type::Fun(fun), Some(resource)) = (&export.ty, resource) {
                if takes_sql(fun) {
                    out.push(format!("exports.{}:{}", resource.name, export.name));
                }
            }
        }
    }
    out.retain(|name| is_call_path(name));
    out.sort();
    out.dedup();
    out
}

/// Whether the first value a call passes goes to a parameter of the `sql` type, also an optional
/// one, as `sql?`.
fn takes_sql(fun: &FunType) -> bool {
    fun.params.first().is_some_and(|param| is_sql(&param.ty))
}

fn is_sql(ty: &Type) -> bool {
    match ty {
        Type::Named(name, args) => name.as_str() == "sql" && args.is_empty(),
        Type::Union(types) => types.iter().any(is_sql) && types.iter().all(|ty| *ty == Type::Nil || is_sql(ty)),
        _ => false,
    }
}

/// Whether `name` is a path of identifiers, which an editor can match where a call writes it. A
/// resource name may hold `-`, as `exports.my-resource:fetch` does.
fn is_call_path(name: &str) -> bool {
    let (path, method) = name.split_once(':').map_or((name, None), |(path, method)| (path, Some(method)));
    let identifier = |part: &str| {
        part.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let mut parts = path.split('.');
    let first = parts.next().unwrap_or_default();
    let resource_part =
        |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let rest_ok = match first {
        "exports" => parts.next().is_some_and(resource_part) && parts.next().is_none(),
        _ => parts.all(identifier),
    };
    identifier(first) && rest_ok && method.is_none_or(identifier)
}
