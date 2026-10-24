//! How FiveM's Lua runtime passes the arguments of a call of a native. The generated Lua wrapper of
//! the native (ext/natives/codegen_out_lua.lua) binds them to its parameters by position, converts
//! some of them and adds the pointers it fills itself, and the runtime then spreads each vector over
//! as many argument slots as it has parts (`PushArgument` in LuaScriptNatives.cpp). A vector passed
//! for the `x` of `x, y, z` so fills all three, as long as nothing the wrapper passes after it sits
//! at a fixed place or is converted for another parameter than the one it reaches. The fxv2 natives
//! that `use_experimental_fxv2_oal` turns on read each argument as its parameter and spread nothing.

use crate::Native;

/// How the Lua wrapper of a native passes a parameter on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// As it is given.
    Value,
    /// A `Hash`, which the wrapper hashes when it is given a string.
    Hash,
    /// A string, which the wrapper passes as text, and `0` and `false` as nil.
    String,
    /// A function, which the wrapper passes as a reference.
    Function,
    /// A table, which the wrapper packs, passing its data and its length.
    Object,
    /// The native's one pointer, when it is its last parameter: what is given is its initial value.
    Pointer,
    /// A pointer that the wrapper fills itself, which takes no argument.
    Filled,
}

/// A parameter of a native as its Lua wrapper passes it on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapperParam {
    pub name: &'static str,
    pub ty: &'static str,
    pub optional: bool,
    pub kind: ParamKind,
}

/// A parameter of a native as `Citizen.InvokeNative` passes it; see `Native::invoke_params`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvokeParam {
    pub name: &'static str,
    pub ty: &'static str,
    /// Whether a call may leave it out, which FiveM then fills with 0.
    pub optional: bool,
    /// What it takes, as the Lua wrapper would take it: `Filled` and `Pointer` are pointers, which
    /// take a `Citizen.PointerValue*` marker.
    pub kind: ParamKind,
    /// Whether only the game native takes it, which FiveM does not declare.
    pub extra: bool,
}

impl InvokeParam {
    pub fn is_pointer(&self) -> bool {
        matches!(self.kind, ParamKind::Filled | ParamKind::Pointer)
    }
}

impl WrapperParam {
    /// How many argument slots the native reads for it: three for a `vector3`, and the data and the
    /// length of a table.
    fn slots(&self) -> usize {
        match (self.kind, self.ty) {
            (ParamKind::Object, _) => 2,
            (ParamKind::Value, "vector3") => 3,
            _ => 1,
        }
    }

    /// Whether a vector given for it means its parts, for it and the parameters after it.
    pub fn takes_parts(&self) -> bool {
        self.kind == ParamKind::Value && matches!(self.ty, "number" | "any")
    }
}

/// What an argument passed to a native is, as far as the way the runtime passes it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    /// A vector with this many parts.
    Vector(u8),
    String,
    Function,
    /// `nil`, `false` or `0`, which the runtime passes as 0, and the wrapper as nil for a string.
    Zero,
    /// A number, a boolean or another value that is no string.
    Value,
    /// A value of no known kind, taken to be passed as it is.
    Unknown,
}

/// What an argument slot of a native gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Passed {
    Nil,
    /// Argument `i`, `nil`, `false` or `0`, which the runtime passes as it passes nil.
    Zero(usize),
    /// Argument `i` as it is given.
    Arg(usize),
    /// Part `part` of the vector given as argument `i`.
    Part(usize, u8),
    /// Argument `i` hashed.
    Hashed(usize),
    /// Argument `i` as text.
    Text(usize),
    /// A reference to the function given as argument `i`, or to none.
    Reference(Option<usize>),
    /// The data, or the length, that argument `i`, or nil, is packed into.
    Packed(Option<usize>, u8),
    /// The pointer for the parameter at this index of `Native::wrapper_params`.
    Pointer(usize),
}

impl Passed {
    /// What the native gets: `nil`, `false` and `0` alike.
    fn passed(self) -> Passed {
        match self {
            Passed::Zero(_) => Passed::Nil,
            passed => passed,
        }
    }
}

/// A parameter of a native that a call passes another value than it means, as a vector spread
/// before it moves what comes after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Misplaced {
    /// The first argument whose parts are spread.
    pub vector: usize,
    /// The index of the parameter in `Native::wrapper_params`.
    pub param: usize,
    /// What the runtime passes to it.
    pub gets: Passed,
    /// What the call means to pass to it.
    pub means: Passed,
}

/// Where the arguments of a call of a native go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spread {
    /// The index in `Native::params` of the parameter each argument goes to as the call means it: a
    /// vector given for a number parameter takes one parameter for each of its parts.
    pub targets: Vec<usize>,
    /// How many of `Native::params` the arguments fill, as the call means them.
    pub filled: usize,
    /// The first parameter that the runtime passes another value than the call means, when there
    /// is one.
    pub misplaced: Option<Misplaced>,
}

/// What a parameter the wrapper takes is given.
#[derive(Clone, Copy)]
enum Bound {
    /// The argument at this index.
    Whole(usize),
    /// A part of the vector given as an argument.
    Part(usize, u8),
}

impl Native {
    /// Where the generated Lua wrapper of the native and the runtime pass `args`, the kinds of the
    /// arguments of a call. Each argument goes to the next parameter, and a vector given for a
    /// number parameter covers one for each of its parts, which the runtime passes when it does
    /// not move a parameter after the vector: the wrapper binds the arguments before the runtime
    /// spreads the vector, so each argument after it is converted as the parameter it is bound to,
    /// and the pointers the wrapper fills stay where they are.
    pub fn spread(&self, args: &[ArgKind]) -> Spread {
        let layout: Vec<WrapperParam> = self.wrapper_params().collect();
        let taken: Vec<WrapperParam> = layout.iter().copied().filter(|p| p.kind != ParamKind::Filled).collect();
        let mut targets = Vec::with_capacity(args.len());
        let mut means: Vec<Option<Bound>> = vec![None; taken.len()];
        let mut spread = None;
        let mut cursor = 0;
        for (i, kind) in args.iter().enumerate() {
            targets.push(cursor);
            match kind {
                ArgKind::Vector(parts) if taken.get(cursor).is_some_and(WrapperParam::takes_parts) => {
                    spread.get_or_insert(i);
                    for part in 0..*parts {
                        if let Some(bound) = means.get_mut(cursor + usize::from(part)) {
                            *bound = Some(Bound::Part(i, part));
                        }
                    }
                    cursor += usize::from(*parts);
                }
                _ => {
                    if let Some(bound) = means.get_mut(cursor) {
                        *bound = Some(Bound::Whole(i));
                    }
                    cursor += 1;
                }
            }
        }
        let Some(vector) = spread else { return Spread { targets, filled: cursor, misplaced: None } };
        let meant = slots(&layout, args, |position| means[position]);
        let given = slots(&layout, args, |position| (position < args.len()).then_some(Bound::Whole(position)));
        let owners = layout.iter().enumerate().flat_map(|(index, param)| std::iter::repeat_n(index, param.slots()));
        let misplaced = owners.enumerate().find_map(|(slot, param)| {
            let gets = given.get(slot).copied().unwrap_or(Passed::Nil);
            let means = meant.get(slot).copied().unwrap_or(Passed::Nil);
            (gets.passed() != means.passed()).then_some(Misplaced { vector, param, gets, means })
        });
        Spread { targets, filled: cursor, misplaced }
    }
}

/// The argument slots that the wrapper of a native with the parameters `layout` passes, when each
/// parameter it takes, by its position among those, is given what `bound` tells.
fn slots(layout: &[WrapperParam], args: &[ArgKind], bound: impl Fn(usize) -> Option<Bound>) -> Vec<Passed> {
    let mut out = Vec::new();
    let mut position = 0;
    for (index, param) in layout.iter().enumerate() {
        if param.kind == ParamKind::Filled {
            out.push(Passed::Pointer(index));
            continue;
        }
        let value = bound(position);
        position += 1;
        let whole = match value {
            Some(Bound::Whole(i)) => Some(i),
            _ => None,
        };
        match (param.kind, value) {
            (ParamKind::Pointer, _) => out.push(Passed::Pointer(index)),
            (_, Some(Bound::Part(i, part))) => out.push(Passed::Part(i, part)),
            (ParamKind::Object, _) => out.extend([Passed::Packed(whole, 0), Passed::Packed(whole, 1)]),
            (ParamKind::Function, _) => out.push(Passed::Reference(whole)),
            (_, None) => out.push(Passed::Nil),
            (kind, Some(Bound::Whole(i))) => match (kind, args[i]) {
                (_, ArgKind::Zero) => out.push(Passed::Zero(i)),
                (ParamKind::String, ArgKind::String | ArgKind::Unknown) => out.push(Passed::Arg(i)),
                (ParamKind::String, _) => out.push(Passed::Text(i)),
                (ParamKind::Hash, ArgKind::String) => out.push(Passed::Hashed(i)),
                (_, ArgKind::Vector(parts)) => out.extend((0..parts).map(|part| Passed::Part(i, part))),
                _ => out.push(Passed::Arg(i)),
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native;

    fn spread(name: &str, args: &[ArgKind]) -> Spread {
        native(name).unwrap().spread(args)
    }

    #[test]
    fn vectors_fill_the_number_parameters_they_are_given_for() {
        use ArgKind::{Value, Vector};
        let coords = spread("SetEntityCoords", &[Value, Vector(3), Value, Value, Value, Value]);
        assert_eq!(coords, Spread { targets: vec![0, 1, 4, 5, 6, 7], filled: 8, misplaced: None });
        // Two vectors, as `GetDistanceBetweenCoords(a, b, true)` passes them.
        let distance = spread("GetDistanceBetweenCoords", &[Vector(3), Vector(3), Value]);
        assert_eq!(distance, Spread { targets: vec![0, 3, 6], filled: 7, misplaced: None });
        // A vector for a parameter of another type is no spread: it goes to that parameter.
        let entity = spread("SetEntityCoords", &[Vector(3), Value]);
        assert_eq!((entity.targets, entity.misplaced), (vec![0, 1], None));
    }

    #[test]
    fn a_spread_that_moves_what_the_wrapper_passes_after_it_is_misplaced() {
        use ArgKind::{String, Value, Vector, Zero};
        // `GetGroundZFor_3dCoord(x, y, z, *groundZ, includeWater)`: the wrapper passes the pointer
        // after `z`, where the spread `vector3` puts `false`.
        let ground = spread("GetGroundZFor_3dCoord", &[Vector(3), Value]);
        let misplaced = ground.misplaced.unwrap();
        assert_eq!((misplaced.vector, misplaced.gets, misplaced.means), (0, Passed::Arg(1), Passed::Pointer(3)));
        let layout: Vec<_> = native("GetGroundZFor_3dCoord").unwrap().wrapper_params().collect();
        assert_eq!((layout[3].name, layout[3].kind), ("groundZ", ParamKind::Filled));
        // A model name reaches `modelHash` through `z`, which does not hash it.
        let object = spread("GetClosestObjectOfType", &[Vector(3), Value, String, Value]);
        let misplaced = object.misplaced.unwrap();
        assert_eq!((misplaced.gets, misplaced.means), (Passed::Arg(2), Passed::Hashed(2)));
        // `false` reaches `drawOnEnts` through the string `textureDict`, as the nil it turns into,
        // which the runtime passes as 0 as well.
        let marker = [vec![Value, Vector(3)], vec![Value; 13], vec![Zero, Value, Value, Zero, Zero, Zero, Zero]];
        assert_eq!(spread("DrawMarker", &marker.concat()).misplaced, None);
        // A hash given as a number is passed as it is either way.
        assert_eq!(spread("GetClosestObjectOfType", &[Vector(3), Value, Value, Value]).misplaced, None);
    }
}
