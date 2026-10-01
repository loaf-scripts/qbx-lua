use qbx_fivem_data::Side;
use qbx_lua_analysis::crossref::trigger_target;
use qbx_lua_syntax::ast::Expr;

use crate::callback_wrappers::{self, Wrapper};
use crate::document::Document;
use crate::framework_callbacks;
use crate::index::{EventDef, EventFamily, EventKind, FileEntry};
use crate::infer::Infer;
use crate::types::{CallbackRole, FunType, Param, Type};
use crate::workspace::Workspace;

pub struct EventCall {
    pub fun: FunType,
    pub event: String,
    /// Where the handler whose parameters are shown lives, e.g. `server/main.lua:12`.
    pub handler_location: String,
}

/// Leading arguments of `lib.callback` style calls that are not passed on to the handler.
fn callback_skip(call: &str) -> Option<usize> {
    match call {
        "lib.callback.await" => Some(2),
        "lib.callback" => Some(3),
        _ => None,
    }
}

/// For `TriggerServerEvent('name', ...)` and friends: the signature of the call with the payload
/// parameters taken from the handler registered for that event.
pub fn event_call(
    ws: &Workspace,
    doc: &Document,
    infer: &Infer,
    callee: &Expr,
    args: &[Expr],
    native: Option<&FunType>,
) -> Option<EventCall> {
    let call = callee.dotted_path()?;
    let name = args.first()?.as_string()?;
    let framework = framework_callbacks::classify(infer.ctx, &ws.index, callee);
    if framework.is_none() {
        native?;
        if trigger_target(&call, None).is_none() && callback_skip(&call).is_none() {
            return None;
        }
    }
    let own_side = ws.index.file(doc.file).and_then(|f| f.side);
    let own_side =
        qbx_lua_analysis::side_guard::SideRegions::of(&doc.text, &doc.chunk).effective(callee.span.start, own_side);

    let (target, skip, wanted, family) = if let Some(framework) = framework {
        if framework.kind != EventKind::Trigger || own_side != Some(framework.required_side()) {
            return None;
        }
        (Some(Side::Server), 2, [EventKind::Callback, EventKind::Callback], framework.family)
    } else {
        // Keep the existing native/ox_lib behavior dependent on a known callee signature.
        native?;
        match trigger_target(&call, own_side) {
            Some((target, skip)) => (target, skip, [EventKind::NetEvent, EventKind::Handler], EventFamily::Native),
            None => {
                let target = own_side.map(|side| if side == Side::Server { Side::Client } else { Side::Server });
                (target, callback_skip(&call)?, [EventKind::Callback, EventKind::Callback], EventFamily::OxLib)
            }
        }
    };

    let framework_callback = matches!(family, EventFamily::QbCore | EventFamily::Esx);
    let candidates = ws
        .index
        .events()
        .filter(|(_, e)| e.name == *name && e.family == family && wanted.contains(&e.kind))
        .filter(|(_, e)| framework_callback || e.handler.is_some())
        .filter(|(_, e)| !matches!((target, e.side), (Some(target), Some(side)) if !side.is_available_on(target)));
    let candidates: Vec<_> = candidates.collect();
    if framework_callback && candidates.iter().any(|(_, event)| event.handler.is_none()) {
        return None;
    }
    let (file, event) = candidates.iter().copied().max_by_key(
        |(_, event)| matches!((target, event.side), (Some(target), Some(side)) if side.is_available_on(target)),
    )?;
    let handler = event.handler.as_deref()?;
    let entry = ws.index.file(file)?;

    // Server callbacks receive the calling player as their first parameter.
    let is_callback = event.kind == EventKind::Callback;
    let handler_skip = if framework_callback {
        2 // Both frameworks supply source and the response callback before the payload.
    } else {
        usize::from(is_callback && event.side != Some(Side::Client) && !handler.params.is_empty())
    };
    if framework_callback
        && candidates.iter().any(|(_, other)| {
            other.handler.as_ref().is_some_and(|other| {
                !other.params.iter().skip(handler_skip).eq(handler.params.iter().skip(handler_skip))
            })
        })
    {
        // Navigation may list all handlers; payload hints must not choose conflicting ones arbitrarily.
        return None;
    }

    let mut params: Vec<Param> = native.into_iter().flat_map(|fun| fun.params.iter().take(skip).cloned()).collect();
    while params.len() < skip {
        let (name, ty) = if framework_callback {
            if params.is_empty() {
                ("name".into(), Type::String)
            } else {
                ("cb".into(), Type::Function)
            }
        } else {
            (format!("arg{}", params.len() + 1).into(), Type::Unknown)
        };
        params.push(Param { name, ty, ..Param::default() });
    }
    params.extend(handler.params.iter().skip(handler_skip).cloned());

    Some(EventCall {
        fun: FunType {
            params,
            returns: if framework_callback {
                // The handler responds through cb; its own return value is not the trigger's return value.
                native.map(|fun| fun.returns.clone()).unwrap_or_default()
            } else if is_callback {
                handler.returns.clone()
            } else {
                Vec::new()
            },
            ..FunType::default()
        },
        event: name.to_string(),
        handler_location: handler_location(ws, entry, event),
    })
}

fn handler_location(ws: &Workspace, entry: &FileEntry, event: &EventDef) -> String {
    let file_name = entry.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let resource = entry.resource.and_then(|id| ws.index.resource(id)).map(|r| format!("{}/", r.name));
    format!("{}{file_name}:{}", resource.unwrap_or_default(), event.range.start.line + 1)
}

/// For a call to a `---@callback await` or `trigger` wrapper: the wrapper's own parameters up to its
/// `...`, then the payload parameters of the handler registered under the name it passes. `await`
/// returns what the handler returns, and `trigger` passes that to its response function.
pub fn wrapper_call(
    ws: &Workspace,
    infer: &Infer,
    fun: &FunType,
    via_method: bool,
    args: &[Expr],
    at: u32,
) -> Option<EventCall> {
    let wrapper = Wrapper::of(fun, via_method)?;
    if wrapper.tag.role == CallbackRole::Register {
        return None;
    }
    let name = args.get(wrapper.arg(wrapper.name))?.as_string()?;
    let target = callback_wrappers::target_of(infer.side_at(at));
    let (file, event) = callback_wrappers::handler(&ws.index, &wrapper.family(), name, target)?;
    let handler = event.handler.as_deref()?;
    let (skip_params, _) = fun.call_offsets(via_method);
    let own = &fun.params[skip_params..];
    let mut params: Vec<Param> = own[..wrapper.payload.unwrap_or(own.len())].to_vec();
    if wrapper.tag.role == CallbackRole::Trigger && !handler.returns.is_empty() {
        if let Some(param) = wrapper.function.and_then(|i| params.get_mut(i)) {
            param.ty = callback_wrappers::response_function(handler);
        }
    }
    if wrapper.payload.is_some() {
        params.extend(handler.params.iter().skip(callback_wrappers::source_skip(event, handler)).cloned());
    }
    let returns = match wrapper.tag.role {
        CallbackRole::Await if !handler.returns.is_empty() => handler.returns.clone(),
        _ => fun.returns.clone(),
    };
    Some(EventCall {
        fun: FunType { params, returns, is_method: fun.is_method, ..FunType::default() },
        event: name.to_string(),
        handler_location: handler_location(ws, ws.index.file(file)?, event),
    })
}
