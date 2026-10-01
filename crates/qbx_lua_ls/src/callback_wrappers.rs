//! Callback systems a resource defines itself, recognized from the `---@callback` tag on their
//! wrapper functions: `RegisterServerCallback('name', handler)` registers a handler the way
//! `lib.callback.register` does, and `AwaitServerCallback('name', ...)` calls it.

use std::sync::Arc;

use qbx_fivem_data::Side;
use smol_str::SmolStr;

use crate::index::{EventDef, EventFamily, EventKind, FileId, Index};
use crate::types::{CallbackRole, CallbackTag, FunType, Param, Type};

/// Where a call to a `@callback` wrapper passes each part, as indexes into the wrapper's parameters
/// after the `self` a `:` call leaves out.
pub struct Wrapper<'f> {
    pub tag: &'f CallbackTag,
    /// The callback name: the first `string` parameter, or else the first that takes no function.
    pub name: usize,
    /// The handler of `register`, or the function `trigger` passes the response to.
    pub function: Option<usize>,
    /// The `...` that `await` and `trigger` pass on to the handler.
    pub payload: Option<usize>,
    /// How many arguments come before the first parameter, as `FunType::call_offsets` counts them.
    pub skip_args: usize,
}

pub fn takes_function(ty: &Type) -> bool {
    ty.as_fun().is_some() || matches!(ty.without_nil(), Type::Function)
}

impl<'f> Wrapper<'f> {
    pub fn of(fun: &'f FunType, via_method: bool) -> Option<Self> {
        let tag = fun.callback.as_ref()?;
        let (skip_params, skip_args) = fun.call_offsets(via_method);
        let params = fun.params.get(skip_params..)?;
        let is_named = |p: &Param| p.name != "...";
        let name = params
            .iter()
            .position(|p| is_named(p) && matches!(p.ty.without_nil(), Type::String | Type::StringLit(_)))
            .or_else(|| params.iter().position(|p| is_named(p) && !takes_function(&p.ty)))?;
        let function = params.iter().position(|p| is_named(p) && takes_function(&p.ty));
        let payload = params.iter().position(|p| !is_named(p));
        if tag.role != CallbackRole::Await && function.is_none() {
            return None;
        }
        Some(Self { tag, name, function, payload, skip_args })
    }

    pub fn family(&self) -> EventFamily {
        EventFamily::Custom(self.tag.family.clone())
    }

    /// The argument at parameter `param`.
    pub fn arg(&self, param: usize) -> usize {
        param + self.skip_args
    }
}

/// The family names of the `@callback` wrappers that globals and table fields hold, sorted.
pub fn families(index: &Index) -> Vec<SmolStr> {
    let mut out: Vec<SmolStr> = Vec::new();
    for (_, file) in index.files() {
        let symbols = file.index.globals.iter().chain(file.index.members.iter().map(|member| &member.symbol));
        for tag in symbols.filter_map(|symbol| symbol.ty.as_fun()?.callback.as_ref()) {
            if !tag.family.is_empty() && !out.contains(&tag.family) {
                out.push(tag.family.clone());
            }
        }
    }
    out.sort();
    out
}

/// The side whose handlers a call made on `side` reaches: the other one, as with `lib.callback`.
pub fn target_of(side: Option<Side>) -> Option<Side> {
    match side {
        Some(Side::Client) => Some(Side::Server),
        Some(Side::Server) => Some(Side::Client),
        _ => None,
    }
}

/// The registration of `name` in `family` whose handler a call aimed at `target` runs, preferring
/// one registered on that side.
pub fn handler<'i>(
    index: &'i Index,
    family: &EventFamily,
    name: &str,
    target: Option<Side>,
) -> Option<(FileId, &'i EventDef)> {
    let reaches =
        |side: Option<Side>| matches!((target, side), (Some(target), Some(side)) if side.is_available_on(target));
    handlers(index, family, name, target).into_iter().max_by_key(|(_, e)| reaches(e.side))
}

/// Every registration of `name` in `family` whose handler a call aimed at `target` may run.
pub fn handlers<'i>(
    index: &'i Index,
    family: &EventFamily,
    name: &str,
    target: Option<Side>,
) -> Vec<(FileId, &'i EventDef)> {
    index
        .events()
        .filter(|(_, e)| e.name == name && e.family == *family && e.kind == EventKind::Callback && e.handler.is_some())
        .filter(|(_, e)| !matches!((target, e.side), (Some(target), Some(side)) if !side.is_available_on(target)))
        .collect()
}

/// Handlers registered outside the client receive the calling player before the payload.
pub fn source_skip(event: &EventDef, handler: &FunType) -> usize {
    usize::from(event.side != Some(Side::Client) && !handler.params.is_empty())
}

/// The function `trigger` passes the response to, taking what the handler returns.
pub fn response_function(handler: &FunType) -> Type {
    let params = handler
        .returns
        .iter()
        .enumerate()
        .map(|(i, ty)| Param {
            name: if i == 0 { "response".into() } else { format!("response{}", i + 1).into() },
            ty: ty.clone(),
            ..Param::default()
        })
        .collect();
    Type::Fun(Arc::new(FunType { params, ..FunType::default() }))
}
