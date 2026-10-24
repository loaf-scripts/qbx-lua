//! Calls of `Citizen.InvokeNative`, which calls a native by its hash and passes each argument after
//! it as it is given, as `PushArgument` in LuaScriptNatives.cpp pushes it: a vector as its parts,
//! and the marker that a `Citizen.*` helper returns as what it marks (`PushMetaPointer` in
//! ScriptInvoker.cpp). A `Citizen.PointerValue*` marker is a pointer argument whose value the call
//! returns, and a `Citizen.ResultAs*` marker takes no argument but tells what the result of the
//! native is returned as. Without a marker, the call returns the result as an integer, or `false`
//! for 0.

use qbx_fivem_data::{native_by_hash, Native};
use qbx_lua_syntax::ast::{Expr, ExprKind};
use qbx_lua_syntax::NumberValue;

/// The functions that call a native by its hash. Only the client has `Citizen.InvokeNative2`.
pub const INVOKERS: &[&str] = &["Citizen.InvokeNative", "Citizen.InvokeNative2"];

/// What a value that `Citizen.InvokeNative` returns holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Integer,
    Float,
    String,
    Vector,
    /// What the unpacker that `Citizen.ResultAsObject2` is given makes of the result.
    Object,
}

/// What a marker argument of `Citizen.InvokeNative` marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// `Citizen.ResultAsInteger()` and the like: the result of the native is returned as such a
    /// value, ahead of those of the pointers.
    Result(Value),
    /// `Citizen.ReturnResultAnyway()`: the result is returned ahead of the values of the pointers,
    /// as an integer, or `false` for 0.
    ResultAnyway,
    /// `Citizen.PointerValueInt()` and the like: a pointer argument, whose value the call returns.
    Pointer(Value),
}

impl Marker {
    /// How many arguments of the native it fills: one for a pointer, and none for a result.
    pub fn slots(self) -> usize {
        usize::from(matches!(self, Marker::Pointer(_)))
    }
}

/// The marker that `arg` passes, when it is a call of one of the `Citizen.*` helpers that make one.
pub fn marker(arg: &Expr) -> Option<Marker> {
    let ExprKind::Call { callee, .. } = &arg.unparen().kind else { return None };
    Some(match callee.dotted_path()?.strip_prefix("Citizen.")? {
        "ResultAsInteger" | "ResultAsLong" => Marker::Result(Value::Integer),
        "ResultAsFloat" => Marker::Result(Value::Float),
        "ResultAsString" => Marker::Result(Value::String),
        "ResultAsVector" => Marker::Result(Value::Vector),
        "ResultAsObject2" => Marker::Result(Value::Object),
        "ReturnResultAnyway" => Marker::ResultAnyway,
        "PointerValueInt" | "PointerValueIntInitialized" => Marker::Pointer(Value::Integer),
        "PointerValueFloat" | "PointerValueFloatInitialized" => Marker::Pointer(Value::Float),
        "PointerValueVector" => Marker::Pointer(Value::Vector),
        _ => return None,
    })
}

/// Whether `callee` is one of the `INVOKERS`.
pub fn is_invoker(callee: &Expr) -> bool {
    callee.dotted_path().is_some_and(|path| INVOKERS.contains(&path.as_str()))
}

/// The native that a call of `callee` with `args` invokes: one of the `INVOKERS` given the hash of
/// a native that the data knows.
pub fn invoked_native(callee: &Expr, args: &[Expr]) -> Option<Native> {
    if !is_invoker(callee) {
        return None;
    }
    native_by_hash(literal_hash(args.first()?)?)
}

/// The hash that `arg` writes out: a number, as `0x49733E92263139D1`, or a string that Lua reads as
/// one, as `'0x49733E92263139D1'`.
pub fn literal_hash(arg: &Expr) -> Option<u64> {
    match &arg.unparen().kind {
        ExprKind::Number(NumberValue::Int(hash)) => Some(*hash as u64),
        ExprKind::String(_) => {
            let text = arg.as_string()?.trim();
            match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => text.parse().ok(),
            }
        }
        _ => None,
    }
}

/// The values that a call of `Citizen.InvokeNative` with `args` returns: the result of the native
/// as a `Citizen.ResultAs*` marker asks for it, or as an integer, unless the call gives a pointer
/// marker without one, and then the value of each pointer. `None` when an argument may be a
/// marker that is not written out, as a call or `...` at the end may give one.
pub fn returned(args: &[Expr]) -> Option<Vec<ReturnedValue>> {
    let (mut result, mut pointers, mut anyway) = (None, Vec::new(), false);
    for (i, arg) in args.iter().enumerate().skip(1) {
        match marker(arg) {
            Some(Marker::Result(value)) => result = Some(value),
            Some(Marker::ResultAnyway) => anyway = true,
            Some(Marker::Pointer(value)) => pointers.push(value),
            None if i + 1 == args.len() && arg.is_multi_value() => return None,
            None => {}
        }
    }
    let first = match result {
        Some(value) => Some(ReturnedValue::As(value)),
        None if pointers.is_empty() || anyway => Some(ReturnedValue::Default),
        None => None,
    };
    Some(first.into_iter().chain(pointers.into_iter().map(ReturnedValue::As)).collect())
}

/// A value that a call of `Citizen.InvokeNative` returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnedValue {
    /// The result of the native as an integer, or `false` for 0.
    Default,
    As(Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `read` reads from the arguments of `f(source)`.
    fn with_args<T>(source: &str, read: impl Fn(&[Expr]) -> T) -> T {
        let chunk = qbx_lua_syntax::parse(&format!("f({source})"));
        let qbx_lua_syntax::ast::StmtKind::Expr(call) = &chunk.block.stmts[0].kind else { panic!() };
        let ExprKind::Call { args, .. } = &call.kind else { panic!() };
        read(args)
    }

    #[test]
    fn hashes_name_natives() {
        let hash = |source: &str| with_args(source, |args| literal_hash(&args[0]));
        assert_eq!(hash("0x49733E92263139D1"), Some(0x49733E92263139D1));
        assert_eq!(hash("'0x49733E92263139D1'"), Some(0x49733E92263139D1));
        // A hex literal past `i64::MAX` wraps around, as Lua reads it.
        assert_eq!(hash("0xC6258F41D86676E0"), Some(0xC6258F41D86676E0));
        assert_eq!(hash("hash"), None);
    }

    #[test]
    fn markers_tell_what_the_call_returns() {
        use ReturnedValue::{As, Default};
        let returns = |source: &str| with_args(source, returned);
        assert_eq!(returns("0x1, ped"), Some(vec![Default]));
        assert_eq!(returns("0x1, Citizen.ResultAsFloat()"), Some(vec![As(Value::Float)]));
        // The values of the pointers, without the result unless a marker asks for it.
        let pointers = "0x1, Citizen.PointerValueInt(), Citizen.PointerValueVector()";
        assert_eq!(returns(pointers), Some(vec![As(Value::Integer), As(Value::Vector)]));
        let anyway = "0x1, Citizen.PointerValueFloat(), Citizen.ReturnResultAnyway()";
        assert_eq!(returns(anyway), Some(vec![Default, As(Value::Float)]));
        let string = "0x1, Citizen.PointerValueIntInitialized(0), Citizen.ResultAsString()";
        assert_eq!(returns(string), Some(vec![As(Value::String), As(Value::Integer)]));
        assert_eq!(returns("0x1, ..."), None);
    }
}
