use lsp_types::{Documentation, ParameterInformation, ParameterLabel, Position, SignatureHelp, SignatureInformation};
use qbx_fivem_data::native_docs;
use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::{ExprKind, Name, StmtKind};

use super::event_call::{event_call, wrapper_call};
use super::hover::{alias_definition, nested_aliases};
use super::{lua_block, markdown, with_infer};
use crate::document::Document;
use crate::indexer::render_doc;
use crate::infer::{Decl, Infer};
use crate::locate::locate;
use crate::types::{FunType, Type};
use crate::workspace::Workspace;

pub fn signature_help(ws: &Workspace, doc: &Document, position: Position) -> Option<SignatureHelp> {
    let offset = doc.offset(position);
    let located = locate(&doc.chunk, offset);
    let site = located.call?;
    with_infer(ws, doc, |infer| {
        let resolved = infer.callee_fun(site.base, site.method);
        let callee = resolved.as_ref().map(|(fun, _)| fun.as_ref());
        let event = site
            .method
            .is_none()
            .then(|| event_call(ws, doc, infer, site.base, site.args, callee))
            .flatten()
            .or_else(|| wrapper_call(ws, infer, callee?, site.method.is_some(), site.args, site.base.span.start));
        let (native, member) = resolved.map(|(fun, member)| (Some(fun), member)).unwrap_or_default();
        let fun = event.as_ref().map(|e| e.fun.clone().into()).or(native)?;
        // A function lists its `@overload`s after the declared signature, with the one the call
        // picks active. An event's handler is its only signature.
        let (signatures, active_signature) = match &event {
            Some(_) => (vec![fun], 0),
            None => infer.call_signatures(&fun, site.args, site.method.is_some(), site.base.span.start),
        };

        let name = match (site.method, &site.base.kind) {
            (Some(method), _) => method.text.to_string(),
            (None, _) => site.base.dotted_path().unwrap_or_default(),
        };

        let mut documentation = member.and_then(|m| m.doc).map(|d| d.to_string());
        if documentation.is_none() {
            if let ExprKind::Name(name) = &site.base.kind {
                documentation = name_doc(ws, doc, infer, name);
            }
        }

        if let Some(event) = &event {
            let note = format!("Parameters of `{}` as handled in `{}`.", event.event, event.handler_location);
            documentation = Some(documentation.map_or(note.clone(), |d| {
                format!(
                    "{note}

{d}"
                )
            }));
        }

        let argument = site.active_argument(&doc.text, offset);
        let signatures: Vec<SignatureInformation> = signatures
            .iter()
            .map(|fun| information(infer, fun, &name, argument, site.method.is_some(), documentation.as_deref()))
            .collect();
        let active_parameter = signatures[active_signature].active_parameter;
        Some(SignatureHelp { signatures, active_signature: Some(active_signature as u32), active_parameter })
    })
}

/// The docs of the function that `name` calls: those of the global or native it names, or the doc
/// comment of the local it names. A local that takes a global, as `local GetEntityCoords =
/// GetEntityCoords` does, and has no doc comment of its own shows the docs of that global.
fn name_doc(ws: &Workspace, doc: &Document, infer: &Infer, name: &Name) -> Option<String> {
    let global = match doc.resolution.resolve_at(name.span.start) {
        Some(Resolved::Local(id)) => {
            let (stmt, value) = match infer.ctx.decl(doc.resolution.local(id).decl.start)? {
                Decl::LocalFunction { stmt, .. } => (stmt, None),
                Decl::Local { stmt, index } => match &stmt.kind {
                    StmtKind::Local { exprs, .. } => (stmt, exprs.get(*index)),
                    _ => return None,
                },
                _ => return None,
            };
            if let Some(own) = render_doc(&infer.ctx.doc_at(stmt.span.start)) {
                return Some(own.to_string());
            }
            match value.map(|value| &value.kind) {
                Some(ExprKind::Name(global))
                    if matches!(doc.resolution.resolve_at(global.span.start), Some(Resolved::Global(_))) =>
                {
                    global
                }
                _ => return None,
            }
        }
        _ => name,
    };
    ws.index
        .globals_named(&global.text, doc.file)
        .into_iter()
        .find_map(|(_, s)| s.doc.as_ref().map(|d| d.to_string()))
        .or_else(|| native_docs(&global.text))
}

/// One signature of a call, with the parameter that the argument at index `argument` is passed to.
/// Each parameter writes out the aliases its type uses, as `type Mode = "a"|"b"` for `mode: Mode`.
fn information(
    infer: &Infer,
    fun: &FunType,
    name: &str,
    argument: usize,
    via_method: bool,
    documentation: Option<&str>,
) -> SignatureInformation {
    let (skip_params, skip_args) = fun.call_offsets(via_method);
    let params: Vec<_> = fun.params.iter().skip(skip_params).collect();
    let labels: Vec<String> = params.iter().map(|p| p.to_string()).collect();
    let mut label = format!("{name}({})", labels.join(", "));
    if !fun.returns.is_empty() {
        label.push_str(&format!(": {}", fun.returns_text()));
    }

    let mut active = argument.saturating_sub(skip_args);
    let is_variadic = params.last().is_some_and(|p| p.name == "...");
    if active >= params.len() && is_variadic {
        active = params.len() - 1;
    }
    SignatureInformation {
        label,
        documentation: documentation.map(|d| Documentation::MarkupContent(markdown(d.to_string()))),
        parameters: Some(
            labels
                .into_iter()
                .zip(&params)
                .map(|(label, param)| ParameterInformation {
                    label: ParameterLabel::Simple(label),
                    documentation: aliases_of(infer, &param.ty),
                })
                .collect(),
        ),
        active_parameter: Some(active as u32),
    }
}

/// The aliases that a parameter of type `ty` uses, written out, if it uses any.
fn aliases_of(infer: &Infer, ty: &Type) -> Option<Documentation> {
    let aliases = nested_aliases(infer, [ty], &[], false);
    if aliases.is_empty() {
        return None;
    }
    let lines: Vec<String> = aliases.iter().map(|(name, alias)| alias_definition(name, alias)).collect();
    Some(Documentation::MarkupContent(markdown(lua_block(&lines.join("\n")))))
}
