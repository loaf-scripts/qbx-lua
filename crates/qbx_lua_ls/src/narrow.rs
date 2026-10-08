//! Type guards and the values locals hold: which of the values given to a local reach each point of
//! the code, and what a condition tells about the locals it tests in the code that only runs when it
//! held or failed. `if not name then return end` leaves `name` holding a value for the rest of its
//! block, `if name then ... end` for the branch, and `name = name or 'none'` gives it a new one.
//! `---@cast` lines change the type of a local from their line on.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use qbx_lua_analysis::scope::{FuncId, LocalId, Resolution, Resolved, MAIN_CHUNK};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{CommentKind, NumberValue, SmolStr, Span, Token};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::luacats::{parse_cast, CastEntry};
use crate::types::Type;

/// The names `type` gives: those of Lua, and those of the vectors, quaternions and matrices CfxLua
/// adds.
const TYPE_NAMES: [&str; 13] = [
    "nil", "boolean", "number", "string", "table", "function", "thread", "userdata", "vector2", "vector3", "vector4",
    "quat", "matrix",
];
/// The names `math.type` gives for numbers.
const NUMBER_NAMES: [&str; 2] = ["integer", "float"];

/// What a guard tells about the value of a local.
#[derive(Clone, Debug, PartialEq)]
pub enum Fact {
    /// It is neither `nil` nor `false`.
    Truthy,
    /// It is `nil` or `false`.
    Falsy,
    /// It equals this `nil`, `true`, `false`, string or integer.
    Is(Type),
    /// It differs from this `nil`, `true`, `false`, string or integer.
    IsNot(Type),
    /// `type` gives this name for it, or `math.type` gives `integer` or `float`.
    Kind(&'static str),
    /// `type` or `math.type` gives another name for it.
    NotKind(&'static str),
    /// The facts of one of these lists hold, as after `type(x) == 'string' or type(x) == 'number'`.
    AnyOf(Vec<Vec<Fact>>),
    /// It equals the value of the local `other` where the comparison reads it, at `at`, as after
    /// `cam == activeCam`. What that rules out depends on the type of `other` there, which
    /// `Infer::facts_left` reads; until then it rules out nothing.
    SameAs { other: LocalId, at: u32 },
    /// It differs from the value of the local `other` where the comparison reads it, at `at`.
    NotSameAs { other: LocalId, at: u32 },
}

impl Fact {
    /// What is left of `ty` for a value the fact holds for, or `None` when no value of `ty`
    /// satisfies it. A type that says nothing about its values stays as it is. The aliases `ty`
    /// names are read as classes, so the caller expands them first.
    pub fn apply(&self, ty: &Type) -> Option<Type> {
        match ty {
            Type::Union(parts) => {
                let kept: Vec<Type> = parts.iter().filter_map(|part| self.apply(part)).collect();
                // The `unknown` of `string|unknown` may be any value the fact holds for.
                let keeps_unknown = kept.contains(&Type::Unknown);
                let ty = (!kept.is_empty()).then(|| Type::union(kept))?;
                Some(if keeps_unknown { ty.or_unknown() } else { ty })
            }
            _ => self.apply_part(ty),
        }
    }

    /// What the code the fact guards takes `ty` to be: what `apply` leaves of it, or, for a `type`
    /// check that `ty` has no value for, the kind it checks for. Such a check is how code handles
    /// values its annotations leave out.
    pub fn assume(&self, ty: &Type) -> Option<Type> {
        self.apply(ty).or_else(|| self.kind_type())
    }

    /// What the code the fact guards takes a value of `ty` to be when the fact holds for none of
    /// them, as lua-language-server reads such code: `unknown` after `type(x) ~= 'table'` for a
    /// table, or after `if x` for a value that is always `nil`; `nil` after `not x` or `x == nil`
    /// for one that never is; and the kind of the literal after `x == 5` for a value that is never a
    /// number. `None` keeps the type whole, as after `x ~= 'a'` for an `'a'`, or `x == 'c'` for an
    /// `'a'|'b'`.
    pub fn ruled_out(&self, ty: &Type) -> Option<Type> {
        match self {
            Fact::NotKind(_) | Fact::Truthy => Some(Type::Unknown),
            Fact::Falsy | Fact::Is(Type::Nil) => Some(Type::Nil),
            Fact::Is(value) => {
                let kind = type_name(value)?;
                let parts = match ty {
                    Type::Union(parts) => &parts[..],
                    one => std::slice::from_ref(one),
                };
                (!parts.iter().any(|part| type_name(part) == Some(kind))).then(|| value.widen())
            }
            _ => None,
        }
    }

    /// The type of the values a `type` check, or each of several, lets through.
    fn kind_type(&self) -> Option<Type> {
        match self {
            Fact::Kind("integer") => Some(Type::Integer),
            Fact::Kind("float") => Some(Type::Number),
            Fact::Kind(name) => Some(Type::named(name)),
            Fact::AnyOf(alternatives) => {
                let kinds: Option<Vec<Type>> =
                    alternatives.iter().map(|facts| facts.iter().rev().find_map(Fact::kind_type)).collect();
                kinds.map(Type::union)
            }
            _ => None,
        }
    }

    fn apply_part(&self, ty: &Type) -> Option<Type> {
        let unknown = matches!(ty, Type::Unknown | Type::Any);
        match self {
            Fact::SameAs { .. } | Fact::NotSameAs { .. } => Some(ty.clone()),
            Fact::AnyOf(alternatives) => {
                let kept: Vec<Type> = alternatives
                    .iter()
                    .filter_map(|facts| facts.iter().try_fold(ty.clone(), |ty, fact| fact.apply(&ty)))
                    .collect();
                (!kept.is_empty()).then(|| Type::union(kept))
            }
            Fact::Kind(_) if unknown => self.kind_type(),
            Fact::Kind(name) => match is_kind(ty, name) {
                Some(false) => None,
                None if *name == "integer" && *ty == Type::Number => Some(Type::Integer),
                _ => Some(ty.clone()),
            },
            Fact::NotKind(name) => (is_kind(ty, name) != Some(true)).then(|| ty.clone()),
            Fact::Is(value) if unknown => Some(value.clone()),
            _ if unknown => Some(ty.clone()),
            _ => match (ty, self) {
                (Type::Boolean, Fact::Truthy) => Some(Type::BooleanLit(true)),
                (Type::Boolean, Fact::Falsy) => Some(Type::BooleanLit(false)),
                (Type::Boolean, Fact::Is(Type::BooleanLit(value))) => Some(Type::BooleanLit(*value)),
                (Type::Boolean, Fact::IsNot(Type::BooleanLit(value))) => Some(Type::BooleanLit(!*value)),
                (Type::Boolean, Fact::Is(_)) => None,
                (Type::Boolean, _) => Some(Type::Boolean),
                // A string or number equal to a literal is that literal. A handle keeps its name.
                (Type::String, Fact::Is(value @ Type::StringLit(_)))
                | (Type::Integer | Type::Number, Fact::Is(value @ Type::IntLit(_))) => Some(value.clone()),
                (Type::Handle(_), Fact::Is(Type::IntLit(_))) => Some(ty.clone()),
                _ => {
                    let falsy = matches!(ty, Type::Nil | Type::BooleanLit(false));
                    let holds = match self {
                        Fact::Truthy => !falsy,
                        Fact::Falsy => falsy,
                        Fact::Is(value) => ty == value,
                        Fact::IsNot(value) => ty != value,
                        _ => true,
                    };
                    holds.then(|| ty.clone())
                }
            },
        }
    }
}

/// Whether `type`, or for `integer` and `float` `math.type`, gives `name` for every value of `ty`
/// (`Some(true)`) or for none of them (`Some(false)`). `None` when it may give it for some, or the
/// kind of `ty` is not known.
fn is_kind(ty: &Type, name: &str) -> Option<bool> {
    let integer = matches!(ty, Type::Integer | Type::IntLit(_) | Type::Handle(_));
    match name {
        "integer" | "float" if integer => Some(name == "integer"),
        "integer" | "float" if *ty == Type::Number => None,
        "integer" | "float" => type_name(ty).map(|_| false),
        _ => type_name(ty).map(|kind| kind == name),
    }
}

/// The name `type` gives for the values of `ty`.
fn type_name(ty: &Type) -> Option<&str> {
    Some(match ty {
        Type::Nil => "nil",
        Type::Boolean | Type::BooleanLit(_) => "boolean",
        Type::Number | Type::Integer | Type::IntLit(_) | Type::Handle(_) => "number",
        Type::String | Type::StringLit(_) => "string",
        Type::Table
        | Type::Array(_)
        | Type::Map(..)
        | Type::Tuple(_)
        | Type::Shape(_)
        | Type::GlobalTable(_)
        | Type::Exports(_) => "table",
        Type::Function | Type::Fun(_) => "function",
        Type::Thread => "thread",
        Type::Userdata => "userdata",
        // Classes describe tables, except for the vectors, quaternions and matrices of CfxLua.
        Type::Named(name, _) => match name.as_str() {
            name @ ("vector2" | "vector3" | "vector4" | "quat" | "matrix") => name,
            _ => "table",
        },
        _ => return None,
    })
}

type Facts = Vec<(LocalId, Fact)>;

/// Whether `expr` is a value that is never true, `nil` or `false`.
fn never_true(expr: &Expr) -> bool {
    matches!(expr.unparen().kind, ExprKind::Nil | ExprKind::False)
}

/// Whether `expr` is a value that is never false: `true`, a number, a string, a table or a function.
fn never_false(expr: &Expr) -> bool {
    matches!(
        expr.unparen().kind,
        ExprKind::True
            | ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::JenkinsHash(_)
            | ExprKind::Table(_)
            | ExprKind::Function(_)
    )
}

/// The index of the declaration of a local among the origins of the values it holds.
pub const DECLARATION: u32 = 0;
/// How many times a loop is walked at most to find what its locals hold at its start.
const MAX_ROUNDS: usize = 4;
/// How many keys a field that guards narrow is read through at most, as the two of `data.job.name`.
const MAX_KEYS: usize = 4;
/// How many locals that hold a condition a guard is followed through at most, as TypeScript follows
/// aliased conditions.
const MAX_ALIAS_DEPTH: u8 = 5;

/// Where a value of a local comes from.
#[derive(Clone, Copy, Debug)]
pub enum Origin<'a> {
    /// The declaration of the local: the value of its `local` statement, or what a parameter or loop
    /// variable is given.
    Declaration,
    /// Value `index` of an assignment, as `name` is value 0 of `name, other = a, b`.
    Assignment { stmt: &'a Stmt, index: usize },
    /// A compound assignment of CfxLua, as `name += 1`.
    Compound { stmt: &'a Stmt },
    /// `function name() end` for a local `name`.
    Function { stmt: &'a Stmt },
    /// Any of the values that the assignments of a set give, as `Flow::others` lists their origins:
    /// what code that runs at other times may have put in the local.
    Others(u32),
    /// The `---@cast` line of this number among `Casts`, which gives the local a value of the type
    /// it names, or of the type the local has there with the types it adds or takes out.
    Cast(u32),
}

/// One of the values a local may hold at a point of the code.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    /// Where it comes from, as an index into the origins of the file.
    pub origin: u32,
    /// What the guards since then tell about it.
    pub facts: FactList,
}

/// What the guards tell about a value, each with where it starts to hold. The list shares what it
/// held before each guard added to it, so a guard adds in constant time and the ways that meet share
/// what they hold in common.
#[derive(Clone, Debug, Default)]
pub struct FactList(Option<Rc<FactNode>>);

#[derive(Debug)]
struct FactNode {
    at: u32,
    fact: Fact,
    /// How many facts the list holds with this one.
    len: usize,
    before: FactList,
}

impl FactList {
    /// The list with `fact` added, holding from `at` on.
    fn with(&self, at: u32, fact: Fact) -> Self {
        FactList(Some(Rc::new(FactNode { at, fact, len: self.len() + 1, before: self.clone() })))
    }

    pub fn len(&self) -> usize {
        self.0.as_ref().map_or(0, |node| node.len)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// Whether `other` is this very list.
    fn is(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        }
    }

    /// The list without the fact added last.
    fn before(&self) -> &Self {
        self.0.as_ref().map_or(self, |node| &node.before)
    }

    /// The facts, the one added last first.
    fn latest(&self) -> impl Iterator<Item = &FactNode> {
        std::iter::successors(self.0.as_deref(), |node| node.before.0.as_deref())
    }

    /// The facts in the order they were added, each with where it starts to hold.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Fact)> {
        let mut nodes: Vec<&FactNode> = self.latest().collect();
        nodes.reverse();
        nodes.into_iter().map(|node| (node.at, &node.fact))
    }
}

impl PartialEq for FactList {
    fn eq(&self, other: &Self) -> bool {
        self.is(other)
            || (self.len() == other.len()
                && self.latest().zip(other.latest()).all(|(a, b)| a.at == b.at && a.fact == b.fact))
    }
}

/// The values a local may hold.
type Values = Rc<[Version]>;

/// The keys that a field of a local is read through, as `job` and `name` for `data.job.name`.
type Keys = Box<[SmolStr]>;

/// What a tracked field is read from: a local, or a global by its name, as `Config` is for
/// `Config.Logs.Service`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Root {
    Local(LocalId),
    Global(SmolStr),
}

/// The root that `name` reads fields from.
fn name_root(resolution: &Resolution, name: &Name) -> Root {
    match resolution.resolve_at(name.span.start) {
        Some(Resolved::Local(local)) => Root::Local(local),
        _ => Root::Global(name.text.clone()),
    }
}

/// Which of the values given to the locals of a file may reach each point of its code, and what the
/// guards on the way tell about them. A value reaches the code after the statement that assigns it,
/// branches join where they meet, and a loop starts with what its runs before leave. Calls are
/// taken to change no local, but a function runs any time after it is created: inside it, a local
/// may also hold what the code after its creation and the other functions assign, and once a
/// function that assigns it is created, the code around may find what any function gives it,
/// unless a runtime function that runs it later, as `SetTimeout` does, is given it. A call
/// that yields, as `Wait(0)` does, lets that code run too, so after one the locals it may assign
/// hold any value it gives them again.
///
/// The fields of locals and globals that guards test or assignments set, as `self.target`,
/// `data.job.name` or `Config.Logs.Service`, are narrowed like locals, an assignment giving one the
/// value it stores, until something may change them: an assignment to the local or global or to a
/// field of the same name of any table; a call that is given, or an assignment for a key that is not
/// known to, the local or global, a table the field is read through, a local copied from one of them
/// or a table built with them; a call that yields; or the start of a function, which runs later.
/// Other calls are taken to change no field, as TypeScript takes them, although the code of another
/// file may assign a global.
#[derive(Debug)]
pub struct Flow<'a> {
    origins: Vec<Origin<'a>>,
    /// The origins of the values that each set of `Origin::Others` stands for, in order.
    others: Vec<Rc<[u32]>>,
    /// The origin of the value each assignment gives a local, by the start of the name it assigns.
    written: FxHashMap<u32, u32>,
    /// The origin of the value each assignment gives a field of a local, as `data.job.name = name`
    /// does, by where its target starts.
    stored: FxHashMap<u32, u32>,
    /// The origins of the values that assignments which read a local give it, in the order they are
    /// written, as `count = count + 1`.
    chained: FxHashMap<LocalId, Vec<u32>>,
    /// Where what each local may hold changes, in order, with what it may hold from there on. Before
    /// the first, it holds the value of its declaration, with nothing known about it.
    points: FxHashMap<LocalId, Vec<(u32, Values)>>,
    declared: Values,
    /// The locals that one call gives values together, as in `local ok, err = f()` or
    /// `ok, err = f()`, each with the origin of its value and the position of that value among those
    /// the call returns.
    links: Vec<Vec<(LocalId, u32, usize)>>,
    link_of: FxHashMap<(LocalId, u32), usize>,
    /// The fields that guards test, by the local or global they are read from, each with the keys
    /// after it and the number it is tracked under, after those of the locals.
    paths: FxHashMap<Root, Vec<(Keys, LocalId)>>,
}

impl<'a> Flow<'a> {
    pub fn of(chunk: &'a Chunk, resolution: &Resolution, casts: &Casts) -> Self {
        let mut found = Origins { resolution, found: Vec::new() };
        found.visit_block(&chunk.block);
        let mut found = found.found;
        // A `---@cast` line gives the local a value, as an assignment does.
        let mut cast_values = Vec::new();
        for (index, cast) in casts.iter().enumerate() {
            let Some(local) = cast.local else { continue };
            found.push((cast.span.start, Origin::Cast(index as u32), Target::None));
            let reads = !cast.replaces();
            let value =
                CastValue { before: cast.before, local, origin: 0, at: cast.span.end, start: cast.span.start, reads };
            cast_values.push(value);
        }
        // Origins are numbered in the order they are written.
        found.sort_by_key(|(at, ..)| *at);
        let mut origins = vec![Origin::Declaration];
        let (mut written, mut stored) = (FxHashMap::default(), FxHashMap::default());
        for (at, origin, target) in found {
            let number = origins.len() as u32;
            match target {
                Target::Local(name) => {
                    written.insert(name, number);
                }
                Target::Field(start) => {
                    stored.insert(start, number);
                }
                Target::None => {}
            }
            if let Some(value) = cast_values.iter_mut().find(|value| value.start == at) {
                value.origin = number;
            }
            origins.push(origin);
        }
        cast_values.sort_unstable_by_key(|value| value.before);
        let flow = Flow {
            origins,
            others: Vec::new(),
            written,
            stored,
            chained: FxHashMap::default(),
            points: FxHashMap::default(),
            declared: Rc::from([Version { origin: DECLARATION, facts: FactList::default() }]),
            links: Vec::new(),
            link_of: FxHashMap::default(),
            paths: FxHashMap::default(),
        };
        let mut walker = Walker::new(chunk, resolution, flow, cast_values);
        walker.frames[0].others = walker.others_of(MAIN_CHUNK);
        walker.block(&chunk.block);
        let mut flow = walker.flow;
        for (index, (root, keys)) in walker.paths.into_inner().into_iter().enumerate() {
            flow.paths.entry(root).or_default().push((keys, walker.first_path + index as LocalId));
        }
        flow
    }

    /// The number that the field `expr` reads is tracked under, when guards test it.
    pub fn path(&self, resolution: &Resolution, expr: &Expr) -> Option<LocalId> {
        if self.paths.is_empty() {
            return None;
        }
        let (root, keys) = field_path(resolution, expr)?;
        self.path_of(&root, &keys)
    }

    /// The number that the field `key` of the table `base` names or reads is tracked under, when
    /// guards test it.
    pub fn member_path(&self, resolution: &Resolution, base: &Expr, key: &str) -> Option<LocalId> {
        if self.paths.is_empty() {
            return None;
        }
        let (root, mut keys) = match &base.unparen().kind {
            ExprKind::Name(name) => (name_root(resolution, name), Vec::new()),
            _ => field_path(resolution, base)?,
        };
        keys.push(SmolStr::new(key));
        self.path_of(&root, &keys)
    }

    fn path_of(&self, root: &Root, keys: &[SmolStr]) -> Option<LocalId> {
        let paths = self.paths.get(root)?;
        paths.iter().find(|(path, _)| **path == *keys).map(|(_, id)| *id)
    }

    /// What `local` may hold at `offset`.
    pub fn at(&self, local: LocalId, offset: u32) -> &[Version] {
        let Some(points) = self.points.get(&local) else { return &self.declared };
        match points.partition_point(|(at, _)| *at <= offset) {
            0 => &self.declared,
            index => &points[index - 1].1,
        }
    }

    pub fn origin(&self, origin: u32) -> Origin<'a> {
        self.origins.get(origin as usize).copied().unwrap_or(Origin::Declaration)
    }

    /// The origins of the values that the set `set` of `Origin::Others` stands for, in order.
    pub fn others(&self, set: u32) -> &[u32] {
        self.others.get(set as usize).map_or(&[], |origins| &origins[..])
    }

    /// The origins of the values that assignments which read `local` give it, in the order they are
    /// written: those whose types may each depend on the one before.
    pub fn chained(&self, local: LocalId) -> &[u32] {
        self.chained.get(&local).map_or(&[], Vec::as_slice)
    }

    /// The origin of the value that an assignment gives the local whose name starts at `offset`.
    pub fn written_at(&self, offset: u32) -> Option<u32> {
        self.written.get(&offset).copied()
    }

    /// What the guards that hold from the start to the end of `code` tell about the value of `origin`
    /// when `local` holds it there, as one that is never assigned again holds that of its
    /// declaration.
    pub fn facts(&self, local: LocalId, origin: u32, code: Span) -> impl Iterator<Item = &Fact> {
        let value = self.at(local, code.end).iter().find(|value| value.origin == origin);
        let facts = value.map(|value| value.facts.iter()).into_iter().flatten();
        facts.filter(move |(at, _)| *at <= code.start).map(|(_, fact)| fact)
    }

    /// The locals that the call which gives `local` the value of `origin` gives values too, itself
    /// included, each with the origin of its value and the position of that value among those the
    /// call returns. A guard on one of them also tells which of the sets of values the call can
    /// return the others come from.
    pub fn linked(&self, local: LocalId, origin: u32) -> Option<&[(LocalId, u32, usize)]> {
        self.link_of.get(&(local, origin)).map(|link| self.links[*link].as_slice())
    }
}

/// What an origin gives a value to.
enum Target {
    /// The local whose name starts here.
    Local(u32),
    /// The field of a local whose target starts here, as `data.job` of `data.job = job`.
    Field(u32),
    None,
}

/// Finds the assignments to locals, which are the origins of the values they hold after their
/// declaration, and those to the fields of locals.
struct Origins<'a, 'r> {
    resolution: &'r Resolution,
    /// Each origin, with where it is written and what it gives the value to.
    found: Vec<(u32, Origin<'a>, Target)>,
}

impl<'a> Origins<'a, '_> {
    fn add(&mut self, name: &Name, origin: Origin<'a>) {
        if let Some(Resolved::Local(_)) = self.resolution.resolve_at(name.span.start) {
            self.found.push((name.span.start, origin, Target::Local(name.span.start)));
        }
    }
}

impl<'a> Visitor<'a> for Origins<'a, '_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match &stmt.kind {
            StmtKind::Assign { targets, .. } => {
                for (index, target) in targets.iter().enumerate() {
                    match &target.kind {
                        ExprKind::Name(name) => self.add(name, Origin::Assignment { stmt, index }),
                        _ if field_path(self.resolution, target).is_some() => {
                            let at = target.span.start;
                            self.found.push((at, Origin::Assignment { stmt, index }, Target::Field(at)));
                        }
                        _ => {}
                    }
                }
            }
            StmtKind::CompoundAssign { target: Expr { kind: ExprKind::Name(name), .. }, .. } => {
                self.add(name, Origin::Compound { stmt });
            }
            StmtKind::Function { name, .. } if name.path.is_empty() && name.method.is_none() => {
                self.add(&name.base, Origin::Function { stmt });
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }
}

/// An assignment to a local.
#[derive(Clone, Copy)]
struct Write {
    start: u32,
    /// The function it is in.
    func: FuncId,
    origin: u32,
}

/// What the locals that changed since a point of the walk hold, with `Flow::declared` for one that
/// holds no more than the value of its declaration with nothing known about it.
type Delta = FxHashMap<LocalId, Values>;

/// A function being walked.
struct Frame {
    func: FuncId,
    /// Where the loops around the code being walked start, outermost first, each with the point of
    /// the trail where it starts.
    loops: Vec<(u32, usize)>,
    /// What the locals hold at the `break`s of each of those loops.
    breaks: Vec<Vec<Delta>>,
    /// The labels of the blocks around the code being walked, innermost last, each with where its
    /// statement starts, and each block with the point of the trail where it starts.
    labels: Vec<(Vec<(SmolStr, u32)>, usize)>,
    /// Where the function being walked inside this one is created, or the outermost loop around
    /// that starts: the assignments of this function from there on may run before it does.
    created: u32,
    /// The locals that code running while the function yields may assign, each with the origin of
    /// the values that code gives it.
    others: Vec<(LocalId, u32)>,
}

impl Frame {
    fn new(func: FuncId) -> Self {
        Self { func, loops: Vec::new(), breaks: Vec::new(), labels: Vec::new(), created: 0, others: Vec::new() }
    }
}

/// The value a `---@cast` line gives its local.
#[derive(Clone, Copy)]
struct CastValue {
    /// Where the token after the line starts, before which the local takes the value.
    before: u32,
    local: LocalId,
    origin: u32,
    /// Where the line ends, from which the local holds the value.
    at: u32,
    /// Where the line starts.
    start: u32,
    /// Whether the type of the value depends on the one the local has there, as for `+T` and `-T`.
    reads: bool,
}

/// The locals that hold a table read from another local or a global, as `local child = self.child`
/// does, each with the tables it was read from, as a root and the keys after it.
type Aliases = FxHashMap<LocalId, Vec<(Root, Vec<SmolStr>)>>;

/// Walks the code of a file in the order it runs and records what its locals may hold. It keeps what
/// they hold where it is, with a trail of what each change replaced: the ways through an `if` or a
/// loop go back along the trail and are joined by what they changed, so the locals they leave alone
/// cost nothing.
struct Walker<'a, 'r> {
    resolution: &'r Resolution,
    flow: Flow<'a>,
    tokens: &'a [Token],
    /// The values the `---@cast` lines of the file give, by where the token after each line starts.
    casts: Vec<CastValue>,
    /// What the locals hold where the walk is, for those that may hold more than the value of their
    /// declaration with nothing known about it.
    state: FxHashMap<LocalId, Values>,
    /// Each change to `state`, as the local and what it held before.
    trail: Vec<(LocalId, Option<Values>)>,
    /// The fields in `state`.
    fields: FxHashSet<LocalId>,
    /// Whether the code being walked can run: not after a `return`, `break`, `goto` or `error()`.
    live: bool,
    /// Whether what the locals hold is recorded, which it is not while a loop is walked to find what
    /// its locals hold at its start.
    record: bool,
    frames: Vec<Frame>,
    /// Each function by where it starts.
    functions: FxHashMap<u32, FuncId>,
    /// The assignments to each local that is assigned again.
    writes: FxHashMap<LocalId, Vec<Write>>,
    /// Where each assignment to a local starts, in order, with the local and the origin of the value.
    ordered: Vec<(u32, LocalId, u32)>,
    /// The locals by where they are declared, in order.
    declarations: Vec<(u32, LocalId)>,
    /// For each function, the locals declared outside it that are assigned again and that it, or a
    /// function in it, reads or assigns.
    captured: FxHashMap<FuncId, Vec<LocalId>>,
    /// For each function, the locals declared outside it that it, or a function in it, assigns.
    inner_writes: FxHashMap<FuncId, Vec<LocalId>>,
    /// For each function, the locals it declares that the functions in it assign.
    nested_writes: FxHashMap<FuncId, Vec<LocalId>>,
    /// Where the assignments to each local in the function that declares it start, in order.
    own_writes: FxHashMap<LocalId, Vec<u32>>,
    /// The origins that stand for what code that runs at other times may give a local, by the local
    /// and how many of the assignments in the function that declares it come before that code.
    others: FxHashMap<(LocalId, usize), Option<u32>>,
    /// The local and that number of each of those origins.
    others_of: FxHashMap<u32, (LocalId, usize)>,
    /// The calls that may yield, by where they start, in order, each with the function it is in.
    yields: Vec<(u32, FuncId)>,
    /// The functions that runtime functions are given to run later, by where they start.
    deferred: FxHashSet<u32>,
    aliases: Aliases,
    /// What the locals hold at the `goto`s that jump ahead to a label, by where its statement starts,
    /// as what changed since the block of the label started.
    gotos: FxHashMap<u32, Vec<Delta>>,
    /// The locals that hold what `type`, or `math.type` with `true`, gives for another local, as
    /// `local kind = type(value)` does, with, for one that is assigned again, the origins of the
    /// values it held there, which comparisons of the kind tell about only while it holds them.
    kinds: FxHashMap<LocalId, (LocalId, bool, Option<Vec<u32>>)>,
    /// The locals that hold `type`, or with `true` `math.type`, as after `local type = type`.
    type_functions: FxHashMap<LocalId, bool>,
    /// The value each local that is never assigned again is declared with, which a guard on the local
    /// tells about, as `local playerName = playerSource and GetPlayerName(playerSource)` does.
    conditions: FxHashMap<LocalId, &'a Expr>,
    /// How many of those locals the guard being read is followed through.
    alias_depth: Cell<u8>,
    /// The fields that guards test, each as the local or global it is read from and the keys after
    /// it, tracked under `first_path` and the numbers after it.
    paths: RefCell<Vec<(Root, Keys)>>,
    path_ids: RefCell<FxHashMap<(Root, Keys), LocalId>>,
    first_path: LocalId,
}

impl<'a, 'r> Walker<'a, 'r> {
    fn new(chunk: &'a Chunk, resolution: &'r Resolution, flow: Flow<'a>, casts: Vec<CastValue>) -> Self {
        let mut writes: FxHashMap<LocalId, Vec<Write>> = FxHashMap::default();
        let mut ordered = Vec::new();
        for (id, local) in resolution.locals.iter().enumerate() {
            for write in local.refs.iter().filter(|r| r.write) {
                let Some(origin) = flow.written_at(write.span.start) else { continue };
                writes.entry(id as LocalId).or_default().push(Write {
                    start: write.span.start,
                    func: write.func,
                    origin,
                });
                ordered.push((write.span.start, id as LocalId, origin));
            }
        }
        ordered.extend(casts.iter().map(|cast| (cast.start, cast.local, cast.origin)));
        ordered.sort_unstable();
        let mut declarations: Vec<(u32, LocalId)> =
            resolution.locals.iter().enumerate().map(|(id, local)| (local.decl.start, id as LocalId)).collect();
        declarations.sort_unstable();
        // The functions between where a local is used and the function that declares it.
        let outside = |from: FuncId, decl: FuncId| {
            std::iter::successors(Some(from), |func| resolution.functions.get(*func as usize).and_then(|f| f.parent))
                .take_while(move |func| *func != decl)
        };
        let mut captured: FxHashMap<FuncId, Vec<LocalId>> = FxHashMap::default();
        let mut inner_writes: FxHashMap<FuncId, Vec<LocalId>> = FxHashMap::default();
        let mut nested_writes: FxHashMap<FuncId, Vec<LocalId>> = FxHashMap::default();
        let mut own_writes: FxHashMap<LocalId, Vec<u32>> = FxHashMap::default();
        let mut locals: Vec<&LocalId> = writes.keys().collect();
        locals.sort_unstable();
        for id in locals {
            let local = resolution.local(*id);
            for reference in &local.refs {
                for func in outside(reference.func, local.func) {
                    let used = captured.entry(func).or_default();
                    if used.last() != Some(id) {
                        used.push(*id);
                    }
                }
            }
            for write in &writes[id] {
                for func in outside(write.func, local.func) {
                    let assigned = inner_writes.entry(func).or_default();
                    if assigned.last() != Some(id) {
                        assigned.push(*id);
                    }
                }
            }
            if writes[id].iter().any(|write| write.func != local.func) {
                nested_writes.entry(local.func).or_default().push(*id);
            }
            let mut own: Vec<u32> =
                writes[id].iter().filter(|write| write.func == local.func).map(|write| write.start).collect();
            own.sort_unstable();
            own_writes.insert(*id, own);
        }
        let functions: FxHashMap<u32, FuncId> =
            resolution.functions.iter().enumerate().skip(1).map(|(id, func)| (func.span.start, id as FuncId)).collect();
        let yields = Yields::of(chunk, resolution, &functions);
        let mut deferred = Deferred { resolution, out: FxHashSet::default() };
        deferred.visit_block(&chunk.block);
        let mut flow = flow;
        let mut chains: FxHashMap<LocalId, Vec<(u32, u32)>> = FxHashMap::default();
        for (local, writes) in &writes {
            let reads = |stmt: &Stmt| {
                let refs = &resolution.local(*local).refs;
                refs.iter().any(|r| !r.write && stmt.span.contains(r.span.start))
            };
            let chained = writes.iter().filter(|write| match flow.origin(write.origin) {
                Origin::Assignment { stmt, .. } | Origin::Compound { stmt } => reads(stmt),
                _ => false,
            });
            chains.entry(*local).or_default().extend(chained.map(|write| (write.start, write.origin)));
        }
        // `+T` and `-T` read the type the local has there.
        for cast in casts.iter().filter(|cast| cast.reads) {
            chains.entry(cast.local).or_default().push((cast.start, cast.origin));
        }
        for (local, mut origins) in chains {
            origins.sort_unstable();
            flow.chained.insert(local, origins.into_iter().map(|(_, origin)| origin).collect());
        }
        Self {
            resolution,
            flow,
            tokens: &chunk.tokens,
            casts,
            state: FxHashMap::default(),
            trail: Vec::new(),
            fields: FxHashSet::default(),
            live: true,
            record: true,
            frames: vec![Frame::new(MAIN_CHUNK)],
            functions,
            writes,
            ordered,
            declarations,
            captured,
            inner_writes,
            nested_writes,
            own_writes,
            others: FxHashMap::default(),
            others_of: FxHashMap::default(),
            yields,
            deferred: deferred.out,
            aliases: Aliases::default(),
            gotos: FxHashMap::default(),
            kinds: FxHashMap::default(),
            type_functions: FxHashMap::default(),
            conditions: FxHashMap::default(),
            alias_depth: Cell::new(0),
            paths: RefCell::default(),
            path_ids: RefCell::default(),
            first_path: resolution.locals.len() as LocalId,
        }
    }

    /// The origin that stands for the values that code other than the function declaring `local`
    /// gives it, with those that function gives it from `created` on: what it may hold in a function
    /// created there, which may run any time later. `None` when there are none.
    fn others(&mut self, local: LocalId, created: u32) -> Option<u32> {
        let from = self.own_writes.get(&local)?.partition_point(|start| *start < created);
        if let Some(origin) = self.others.get(&(local, from)) {
            return *origin;
        }
        let decl = self.resolution.local(local).func;
        let given = self.writes[&local].iter().filter(|write| write.func != decl || write.start >= created);
        let mut origins: Vec<u32> = given.map(|write| write.origin).collect();
        origins.sort_unstable();
        let origin = (!origins.is_empty()).then(|| {
            let origin = self.flow.origins.len() as u32;
            self.flow.origins.push(Origin::Others(self.flow.others.len() as u32));
            self.flow.others.push(origins.into());
            self.others_of.insert(origin, (local, from));
            origin
        });
        self.others.insert((local, from), origin);
        origin
    }

    /// The locals that code running while the function `func` yields may assign, those it declares
    /// that the functions in it assign, each with the origin of the values that code gives it.
    fn others_of(&mut self, func: FuncId) -> Vec<(LocalId, u32)> {
        let locals = self.nested_writes.get(&func).cloned().unwrap_or_default();
        locals.into_iter().filter_map(|local| Some((local, self.others(local, u32::MAX)?))).collect()
    }

    /// `values` where the local may also hold any of the values that `others`, an origin of
    /// `Walker::others`, stands for, with nothing known about them. The values among them are left
    /// out, as are those of the same local that stand for no more.
    fn with_others(&self, values: &Values, others: u32) -> Values {
        let (local, from) = self.others_of[&others];
        let Origin::Others(set) = self.flow.origin(others) else { return values.clone() };
        let origins = self.flow.others(set);
        let covered = |value: &Version| {
            origins.binary_search(&value.origin).is_ok()
                || self.others_of.get(&value.origin).is_some_and(|(of, after)| *of == local && *after >= from)
        };
        let fresh = |value: &Version| value.origin == others && value.facts.is_empty();
        if values.iter().any(fresh) && !values.iter().any(|value| value.origin != others && covered(value)) {
            return values.clone();
        }
        let kept = values.iter().filter(|value| !covered(value)).cloned();
        kept.chain([Version { origin: others, facts: FactList::default() }]).collect()
    }

    /// Goes on after the call `call`, which lets the code that runs while it yields change what the
    /// locals hold, when it may.
    fn call_ended(&mut self, call: &Expr) {
        let first = self.yields.partition_point(|(at, _)| *at < call.span.start);
        if self.yields.get(first).is_some_and(|(at, _)| *at == call.span.start) {
            let yielded = self.yielded(Delta::default());
            self.apply(call.span.end, &yielded);
        }
    }

    /// Whether a call in `code` that the function being walked makes may yield.
    fn yields_in(&self, code: Span) -> bool {
        let func = self.frames.last().map_or(MAIN_CHUNK, |frame| frame.func);
        let first = self.yields.partition_point(|(at, _)| *at < code.start);
        self.yields[first..].iter().take_while(|(at, _)| *at < code.end).any(|(_, of)| *of == func)
    }

    /// `delta`, once the function being walked yields: the locals that the code that runs meanwhile
    /// may assign may hold what it gives them, and the fields of any table may hold anything.
    fn yielded(&self, mut delta: Delta) -> Delta {
        let others = self.frames.last().map_or(&[][..], |frame| &frame.others[..]);
        for (local, origin) in others {
            let values = self.with_others(&self.values_in(&delta, *local), *origin);
            delta.insert(*local, values);
        }
        let fields: Vec<LocalId> =
            self.fields.iter().copied().chain(delta.keys().copied().filter(|id| self.is_path(*id))).collect();
        for field in fields {
            delta.insert(field, self.flow.declared.clone());
        }
        delta
    }

    /// Notes that `local` holds what `value` gives it, which is a table read from another local when
    /// it names one or reads a field of one, or holds those tables when it builds a table with them.
    fn note_alias(&mut self, local: LocalId, value: &Expr) {
        let mut tables = Vec::new();
        tables_of(self.resolution, &self.aliases, value, &mut tables);
        tables.retain(|(root, _)| *root != Root::Local(local));
        if tables.is_empty() {
            return;
        }
        let aliases = self.aliases.entry(local).or_default();
        for table in tables {
            if !aliases.contains(&table) {
                aliases.push(table);
            }
        }
    }

    /// Whether `id` is the number of a field rather than of a local.
    fn is_path(&self, id: LocalId) -> bool {
        id >= self.first_path
    }

    /// The local that `id` is, or the local or global that the field it is the number of is read
    /// from.
    fn root(&self, id: LocalId) -> Root {
        match self.is_path(id) {
            true => self.paths.borrow()[(id - self.first_path) as usize].0.clone(),
            false => Root::Local(id),
        }
    }

    /// Whether `code` declares the local that `root` is, which a global is not.
    fn declares(&self, code: Span, root: &Root) -> bool {
        match root {
            Root::Local(local) => code.contains(self.resolution.local(*local).decl.start),
            Root::Global(_) => false,
        }
    }

    /// The local `expr` names, or the number of the field of a local it reads.
    fn place(&self, expr: &Expr) -> Option<LocalId> {
        if let Some(local) = self.local(expr) {
            return Some(local);
        }
        let (root, keys) = field_path(self.resolution, expr)?;
        let key = (root, keys.into_boxed_slice());
        if let Some(id) = self.path_ids.borrow().get(&key) {
            return Some(*id);
        }
        let id = self.first_path + self.paths.borrow().len() as LocalId;
        self.paths.borrow_mut().push(key.clone());
        self.path_ids.borrow_mut().insert(key, id);
        Some(id)
    }

    /// Forgets what the guards told about the fields that `changed` picks by the local or global
    /// they are read from and the keys after it, from `at` on.
    fn forget_fields(&mut self, at: u32, changed: impl Fn(&Root, &[SmolStr]) -> bool) {
        if self.fields.is_empty() {
            return;
        }
        let paths = self.paths.borrow();
        let fields = self.fields.iter().filter(|id| {
            let (root, keys) = &paths[(**id - self.first_path) as usize];
            changed(root, keys)
        });
        let fields: Vec<LocalId> = fields.copied().collect();
        drop(paths);
        for id in fields {
            self.set(id, at, self.flow.declared.clone());
        }
    }

    /// Forgets what the guards told about the fields of the tables that `expr` names or reads, which
    /// something it is given to may change, also through the locals they were copied to and the
    /// tables built with them.
    fn forget_fields_of(&mut self, expr: &Expr, at: u32) {
        let mut tables = Vec::new();
        tables_of(self.resolution, &self.aliases, expr, &mut tables);
        if tables.is_empty() {
            return;
        }
        self.forget_fields(at, |root, keys| {
            let within = |(table, prefix): &(Root, Vec<SmolStr>)| {
                root == table && keys.len() > prefix.len() && keys.starts_with(prefix)
            };
            tables.iter().any(within)
        });
    }

    /// Forgets what the guards told about the fields that an assignment to `target` may change: those
    /// of the same name, read from any table, or for a key that is not known, those of the table and
    /// of the tables it was copied from.
    fn forget_assigned(&mut self, target: &Expr, at: u32) {
        let key = match &target.unparen().kind {
            ExprKind::Field { name, .. } => Some(name.text.clone()),
            ExprKind::Index { base, index, .. } => match index.as_string() {
                Some(key) => Some(SmolStr::new(key)),
                None => return self.forget_fields_of(base, at),
            },
            _ => None,
        };
        if let Some(key) = key {
            self.forget_fields(at, |_, keys| keys.contains(&key));
        }
    }

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("the main chunk is always walked")
    }

    fn is_declared(values: &[Version]) -> bool {
        matches!(values, [value] if value.origin == DECLARATION && value.facts.is_empty())
    }

    /// What `local` holds where the walk is.
    fn values(&self, local: LocalId) -> Values {
        self.state.get(&local).cloned().unwrap_or_else(|| self.flow.declared.clone())
    }

    /// What `local` holds where the walk is, once the locals that `delta` tells about hold that.
    fn values_in(&self, delta: &Delta, local: LocalId) -> Values {
        delta.get(&local).cloned().unwrap_or_else(|| self.values(local))
    }

    /// Records that `local` may hold `values` from `at` on.
    fn point(&mut self, local: LocalId, at: u32, values: &Values) {
        if !self.record {
            return;
        }
        let Some(points) = self.flow.points.get_mut(&local) else {
            if !Self::is_declared(values) {
                self.flow.points.insert(local, vec![(at, values.clone())]);
            }
            return;
        };
        match points.last_mut() {
            Some((last, held)) if *last == at => *held = values.clone(),
            Some((_, held)) if same(held, values) => {}
            _ => points.push((at, values.clone())),
        }
    }

    /// Makes `local` hold one of `values` from `at` on.
    fn set(&mut self, local: LocalId, at: u32, values: Values) {
        self.point(local, at, &values);
        self.replace(local, values);
    }

    /// Makes `local` hold one of `values`, with what it held before on the trail.
    fn replace(&mut self, local: LocalId, values: Values) {
        let old = match Self::is_declared(&values) {
            true => self.state.remove(&local),
            false => self.state.insert(local, values),
        };
        if old.is_none() && !self.state.contains_key(&local) {
            return;
        }
        self.note_field(local);
        self.trail.push((local, old));
    }

    /// Keeps `fields` telling whether the field `id` is in `state`.
    fn note_field(&mut self, id: LocalId) {
        if !self.is_path(id) {
            return;
        }
        match self.state.contains_key(&id) {
            true => self.fields.insert(id),
            false => self.fields.remove(&id),
        };
    }

    /// The point of the trail the walk is at.
    fn mark(&self) -> usize {
        self.trail.len()
    }

    /// Goes back along the trail to what the locals held at `mark`, from `at` on. Without `at` the
    /// code after it is not walked from there.
    fn rollback(&mut self, mark: usize, at: Option<u32>) {
        let mut restored = Vec::new();
        while self.trail.len() > mark {
            let Some((local, old)) = self.trail.pop() else { break };
            match old {
                Some(values) => self.state.insert(local, values),
                None => self.state.remove(&local),
            };
            self.note_field(local);
            restored.push(local);
        }
        let Some(at) = at else { return };
        restored.sort_unstable();
        restored.dedup();
        for local in restored {
            let values = self.values(local);
            self.point(local, at, &values);
        }
    }

    /// What the locals that changed since `mark` hold.
    fn delta(&self, mark: usize) -> Delta {
        let mut delta = Delta::default();
        for (local, _) in &self.trail[mark..] {
            delta.entry(*local).or_insert_with(|| self.values(*local));
        }
        delta
    }

    /// Makes the locals hold what `delta` tells from `at` on.
    fn apply(&mut self, at: u32, delta: &Delta) {
        for (local, values) in delta {
            self.set(*local, at, values.clone());
        }
    }

    /// `delta` without what tells that a local holds what it holds where the walk is.
    fn settled(&self, mut delta: Delta) -> Delta {
        delta.retain(|local, values| !same(values, &self.values(*local)));
        delta
    }

    /// `values` with `fact` holding for each of them from `at` on.
    fn with_fact(values: &[Version], at: u32, fact: &Fact) -> Values {
        let values =
            values.iter().map(|value| Version { origin: value.origin, facts: value.facts.with(at, fact.clone()) });
        values.collect()
    }

    /// Adds what `cond` being true, or false without `holds`, tells about the locals in it from `at`
    /// on.
    fn assume(&mut self, cond: &Expr, holds: bool, at: u32) {
        let mut facts = Facts::new();
        self.facts(cond, holds, &mut facts);
        for (local, fact) in facts {
            let values = Self::with_fact(&self.values(local), at, &fact);
            self.set(local, at, values);
        }
    }

    /// What changed since `mark`, with what `cond` being true, or false without `holds`, tells from
    /// `at` on.
    fn assuming(&self, mark: usize, cond: &Expr, holds: bool, at: u32) -> Delta {
        let mut delta = self.delta(mark);
        let mut facts = Facts::new();
        self.facts(cond, holds, &mut facts);
        for (local, fact) in facts {
            let values = Self::with_fact(&self.values_in(&delta, local), at, &fact);
            delta.insert(local, values);
        }
        delta
    }

    /// What the locals may hold where the ways in `ways` meet, each as what changed since the
    /// locals held what they hold where the walk is.
    fn joined_ways(&self, ways: &[Delta]) -> Delta {
        let mut locals: Vec<LocalId> = ways.iter().flat_map(|way| way.keys().copied()).collect();
        locals.sort_unstable();
        locals.dedup();
        let mut out = Delta::default();
        for local in locals {
            let held = self.values(local);
            let mut values = ways.iter().map(|way| way.get(&local).unwrap_or(&held));
            let Some(first) = values.next() else { continue };
            out.insert(local, values.fold(first.clone(), |all, values| joined(&all, values)));
        }
        out
    }

    /// Goes on after a statement, from what the ways out of it leave, joined at `at`, each as what
    /// changed since the locals held what they hold where the walk is. With none, the code after it
    /// does not run.
    fn leave(&mut self, at: u32, ways: Vec<Delta>) {
        if ways.is_empty() {
            self.live = false;
            return;
        }
        let joined = self.joined_ways(&ways);
        self.apply(at, &joined);
        self.live = true;
    }

    fn block(&mut self, block: &'a Block) {
        self.statements(block);
        self.forget(block);
    }

    /// Walks the statements of `block` without forgetting the locals it declares, which the
    /// condition of a `repeat` still reads.
    fn statements(&mut self, block: &'a Block) {
        let labels = block.stmts.iter().filter_map(|stmt| match &stmt.kind {
            StmtKind::Label(name) => Some((name.text.clone(), stmt.span.start)),
            _ => None,
        });
        let labels = (labels.collect(), self.mark());
        self.frame().labels.push(labels);
        for (index, stmt) in block.stmts.iter().enumerate() {
            self.cast(stmt.span.start);
            self.stmt(stmt, block, index);
        }
        // A `---@cast` line after the last statement holds for what runs after the block.
        let next = match block.stmts.is_empty() {
            true => block.span.start,
            false => {
                let next = self.tokens.partition_point(|token| token.span.start < block.span.end);
                self.tokens.get(next).map_or(u32::MAX, |token| token.span.start)
            }
        };
        self.cast(next);
        self.frame().labels.pop();
    }

    /// Gives the locals that the `---@cast` lines right before the token starting at `before` name
    /// the values those lines give them.
    fn cast(&mut self, before: u32) {
        let first = self.casts.partition_point(|cast| cast.before < before);
        let count = self.casts[first..].iter().take_while(|cast| cast.before == before).count();
        for index in first..first + count {
            let CastValue { local, origin, at, .. } = self.casts[index];
            self.set(local, at, Rc::from([Version { origin, facts: FactList::default() }]));
            self.forget_fields(at, |root, _| *root == Root::Local(local));
        }
    }

    /// Forgets what the locals that `block` declares hold, as they are gone after it.
    fn forget(&mut self, block: &Block) {
        for stmt in &block.stmts {
            let names: Vec<&Name> = match &stmt.kind {
                StmtKind::Local { names, .. } => names.iter().map(|name| &name.name).collect(),
                StmtKind::LocalFunction { name, .. } => vec![name],
                _ => continue,
            };
            for name in names {
                if let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.span.start) {
                    self.replace(id, self.flow.declared.clone());
                }
            }
        }
    }

    fn stmt(&mut self, stmt: &'a Stmt, block: &'a Block, index: usize) {
        match &stmt.kind {
            StmtKind::Local { names, exprs, in_unpack } => {
                exprs.iter().for_each(|expr| self.visit_expr(expr));
                if !in_unpack {
                    let declared: Vec<Option<(&Name, u32)>> =
                        names.iter().map(|name| Some((&name.name, DECLARATION))).collect();
                    self.link(&declared, exprs);
                    self.note_kinds(names, exprs);
                }
                // A local declared again, as in each run of a loop, holds the value of its declaration.
                for (index, name) in names.iter().enumerate() {
                    if let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.name.span.start) {
                        if self.state.contains_key(&id) {
                            self.set(id, stmt.span.end, self.flow.declared.clone());
                        }
                        self.forget_fields(stmt.span.end, |root, _| *root == Root::Local(id));
                        if let Some(value) = exprs.get(index) {
                            self.note_alias(id, value);
                            if !in_unpack && self.is_constant(id) {
                                self.conditions.insert(id, value);
                            }
                        }
                    }
                }
            }
            StmtKind::LocalFunction { name, func } => {
                if let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.span.start) {
                    if self.state.contains_key(&id) {
                        self.set(id, name.span.start, self.flow.declared.clone());
                    }
                }
                self.function(func);
            }
            StmtKind::Function { name, func } => {
                self.function(func);
                match name.method.as_ref().or(name.path.last()) {
                    Some(key) => {
                        let key = key.text.clone();
                        self.forget_fields(stmt.span.end, |_, keys| keys.contains(&key));
                    }
                    None => self.assigned(&name.base, stmt.span.end),
                }
            }
            StmtKind::Assign { targets, exprs } => {
                exprs.iter().for_each(|expr| self.visit_expr(expr));
                for target in targets.iter().filter(|target| !matches!(target.kind, ExprKind::Name(_))) {
                    self.visit_expr(target);
                }
                let names = targets.iter().map(|target| match &target.kind {
                    ExprKind::Name(name) => self.flow.written_at(name.span.start).map(|origin| (name, origin)),
                    _ => None,
                });
                let names: Vec<Option<(&Name, u32)>> = names.collect();
                self.link(&names, exprs);
                for (index, target) in targets.iter().enumerate() {
                    match &target.kind {
                        ExprKind::Name(name) => {
                            self.assigned(name, stmt.span.end);
                            if let (Some(local), Some(value)) = (self.local(target), exprs.get(index)) {
                                self.note_alias(local, value);
                            }
                        }
                        _ => {
                            self.forget_assigned(target, stmt.span.end);
                            self.store(target, stmt.span.end);
                        }
                    }
                }
            }
            StmtKind::CompoundAssign { target, expr, .. } => {
                self.visit_expr(expr);
                match &target.kind {
                    ExprKind::Name(name) => self.assigned(name, stmt.span.end),
                    _ => {
                        self.visit_expr(target);
                        self.forget_assigned(target, stmt.span.end);
                    }
                }
            }
            StmtKind::Expr(expr) => {
                self.visit_expr(expr);
                if let ExprKind::Call { callee, args, .. } = &expr.kind {
                    if callee.dotted_path().as_deref() == Some("error") {
                        self.live = false;
                    } else if let (ExprKind::Name(name), Some(cond)) = (&callee.kind, args.first()) {
                        // What holds once `assert(cond)` has run.
                        if name.text == "assert" && self.is_global(name) {
                            self.assume(cond, true, stmt.span.end);
                        }
                    }
                }
            }
            StmtKind::Do(body) => self.block(body),
            // It runs when its block ends, which changes nothing for the code after it.
            StmtKind::Defer(body) => {
                let (mark, live) = (self.mark(), self.live);
                self.block(body);
                self.rollback(mark, Some(stmt.span.end));
                self.live = live;
            }
            StmtKind::If { branches, else_block } => self.if_stmt(stmt, branches, else_block.as_ref()),
            StmtKind::While { cond, body } => {
                let (reached, mark) = (self.live, self.mark());
                self.loop_start(stmt, cond.span.start, mark, |walker| walker.while_run(stmt, cond, body, mark).1);
                let (ways, _) = self.while_run(stmt, cond, body, mark);
                self.rollback(mark, Some(stmt.span.end));
                self.leave(stmt.span.end, ways);
                self.live &= reached;
            }
            StmtKind::Repeat { body, cond } => {
                let (reached, mark) = (self.live, self.mark());
                self.loop_start(stmt, body.span.start, mark, |walker| walker.repeat_run(stmt, body, cond, mark).1);
                let (ways, _) = self.repeat_run(stmt, body, cond, mark);
                self.rollback(mark, Some(stmt.span.end));
                self.leave(stmt.span.end, ways);
                self.live &= reached;
            }
            StmtKind::NumericFor { start, limit, step, body, .. } => {
                [Some(start), Some(limit), step.as_ref()].into_iter().flatten().for_each(|expr| self.visit_expr(expr));
                self.for_loop(stmt, body);
            }
            StmtKind::GenericFor { exprs, body, .. } => {
                exprs.iter().for_each(|expr| self.visit_expr(expr));
                self.for_loop(stmt, body);
            }
            StmtKind::Return(exprs) => {
                exprs.iter().for_each(|expr| self.visit_expr(expr));
                self.live = false;
            }
            StmtKind::Break => {
                let mark = self.frames.last().and_then(|frame| frame.loops.last()).map(|(_, mark)| *mark);
                if let Some(mark) = mark.filter(|_| self.live) {
                    let delta = self.delta(mark);
                    if let Some(breaks) = self.frame().breaks.last_mut() {
                        breaks.push(delta);
                    }
                }
                self.live = false;
            }
            StmtKind::Goto(name) => {
                self.goto(stmt, name);
                self.live = false;
            }
            StmtKind::Label(name) => self.label(stmt, name, block, index),
            StmtKind::Error => {}
        }
    }

    /// Notes the locals that hold what `type` gives for another local, or `type` itself.
    fn note_kinds(&mut self, names: &[AttribName], exprs: &[Expr]) {
        for (name, expr) in names.iter().zip(exprs) {
            let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.name.span.start) else { continue };
            if self.is_reassigned(id) {
                continue;
            }
            // `local kind = type(value)` tells nothing about what is assigned to `value` later, or
            // by another function.
            let unchanged =
                |(checked, _): &(LocalId, bool)| !self.is_path(*checked) && !self.assigned_elsewhere(*checked);
            if let Some((checked, numbers)) = self.type_call(expr).filter(unchanged) {
                let held = self.is_reassigned(checked).then(|| origins(&self.values(checked)));
                self.kinds.insert(id, (checked, numbers, held));
            } else if let Some(numbers) = self.type_function(expr) {
                self.type_functions.insert(id, numbers);
            }
        }
    }

    /// Makes the field of a local that `target` names hold the value an assignment gives it from `at`
    /// on.
    fn store(&mut self, target: &Expr, at: u32) {
        let Some(origin) = self.flow.stored.get(&target.span.start).copied() else { return };
        let Some(field) = self.place(target) else { return };
        self.set(field, at, Rc::from([Version { origin, facts: FactList::default() }]));
    }

    /// Makes the local whose name `name` assigns hold the value it is given from `at` on. A global
    /// it assigns holds a table whose fields nothing tells about.
    fn assigned(&mut self, name: &Name, at: u32) {
        let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.span.start) else {
            let global = Root::Global(name.text.clone());
            return self.forget_fields(at, |root, _| *root == global);
        };
        let Some(origin) = self.flow.written_at(name.span.start) else { return };
        let values: Values = Rc::from([Version { origin, facts: FactList::default() }]);
        self.set(id, at, values);
        self.forget_fields(at, |root, _| *root == Root::Local(id));
    }

    fn if_stmt(&mut self, stmt: &'a Stmt, branches: &'a [IfBranch], else_block: Option<&'a Block>) {
        let reached = self.live;
        let else_start = else_block.map(|block| block.span.start.min(stmt.span.end));
        let mark = self.mark();
        let mut ways = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            // The conditions before it failed for the condition of an `elseif` too.
            self.live = reached;
            self.visit_expr(&branch.cond);
            let tested = self.mark();
            // Measured between the keywords so half-typed code inside the branch still counts.
            self.assume(&branch.cond, true, branch.cond.span.end);
            self.block(&branch.block);
            if self.live {
                ways.push(self.delta(mark));
            }
            let next = branches.get(index + 1).map(|branch| branch.keyword_span.end);
            let at = next.or(else_start).unwrap_or(stmt.span.end);
            self.rollback(tested, Some(at));
            self.assume(&branch.cond, false, at);
        }
        match else_block {
            Some(block) => {
                self.live = reached;
                self.block(block);
                if self.live {
                    ways.push(self.delta(mark));
                }
            }
            None if reached => ways.push(self.delta(mark)),
            None => {}
        }
        self.rollback(mark, Some(stmt.span.end));
        self.leave(stmt.span.end, ways);
    }

    /// Makes the locals hold, from `at` on, what they hold at the start of each run of the loop
    /// `stmt`, of which `run` walks one from what they hold at its start, giving what changed since
    /// `mark` where it starts again.
    fn loop_start(&mut self, stmt: &Stmt, at: u32, mark: usize, run: impl Fn(&mut Self) -> Option<Delta>) {
        let entry = self.entry(stmt);
        let first = self.ordered.partition_point(|(start, ..)| *start < stmt.span.start);
        let assigns = self.ordered.get(first).is_some_and(|(start, ..)| *start < stmt.span.end);
        let yields = self.yields_in(stmt.span);
        if !assigns && !yields {
            return self.apply(at, &entry);
        }
        // Inside a loop being walked to find what its locals hold at its start, the loops in it start
        // with any value they give, which keeps loops inside loops from multiplying the walks.
        if !self.record {
            let mut widened = self.widened(stmt.span, entry);
            if yields {
                widened = self.yielded(widened);
            }
            return self.apply(at, &widened);
        }
        let live = self.live;
        self.record = false;
        let mut start = self.settled(entry);
        for round in 1..=MAX_ROUNDS {
            self.apply(at, &start);
            self.live = true;
            let again = run(self);
            self.rollback(mark, None);
            let next = match again {
                Some(again) => {
                    let ways = [start.clone(), self.within(stmt, again)];
                    self.settled(self.joined_ways(&ways))
                }
                None => start.clone(),
            };
            if same_deltas(&next, &start) {
                break;
            }
            start = next;
            if round == MAX_ROUNDS {
                start = self.settled(self.widened(stmt.span, start));
            }
        }
        (self.record, self.live) = (true, live);
        self.apply(at, &start);
    }

    /// What the loop `stmt` starts with: the locals it declares, which each of its runs declares
    /// again, hold the value of their declaration, and the fields it may change before it starts
    /// again are forgotten.
    fn entry(&self, stmt: &Stmt) -> Delta {
        let span = stmt.span;
        let mut entry = Delta::default();
        let first = self.declarations.partition_point(|(start, _)| *start < span.start);
        let inside = self.declarations[first..].iter().take_while(|(start, _)| *start <= span.end);
        for (_, local) in inside.filter(|(start, local)| span.contains(*start) && self.state.contains_key(local)) {
            entry.insert(*local, self.flow.declared.clone());
        }
        if self.fields.is_empty() {
            return entry;
        }
        let mut changes = Changes {
            resolution: self.resolution,
            aliases: &self.aliases,
            locals: Vec::new(),
            globals: Vec::new(),
            keys: Vec::new(),
            tables: Vec::new(),
        };
        visit::walk_stmt(&mut changes, stmt);
        let paths = self.paths.borrow();
        for id in &self.fields {
            let (root, keys) = &paths[(*id - self.first_path) as usize];
            if self.declares(span, root) || changes.changes(root, keys) {
                entry.insert(*id, self.flow.declared.clone());
            }
        }
        entry
    }

    /// `delta` without the locals that the loop `stmt` declares, which each of its runs declares
    /// again.
    fn within(&self, stmt: &Stmt, mut delta: Delta) -> Delta {
        for (local, values) in delta.iter_mut() {
            if self.declares(stmt.span, &self.root(*local)) {
                *values = self.flow.declared.clone();
            }
        }
        delta
    }

    /// `delta` where the locals that `code` assigns may hold any value it gives them, with nothing
    /// known about them, as at the start of a loop when walking it again has not settled what they
    /// hold. Those that `code` declares are left out.
    fn widened(&self, code: Span, mut delta: Delta) -> Delta {
        let first = self.ordered.partition_point(|(start, ..)| *start < code.start);
        let inside = self.ordered[first..].iter().take_while(|(start, ..)| *start < code.end);
        let mut given: FxHashMap<LocalId, Vec<u32>> = FxHashMap::default();
        for (_, local, origin) in
            inside.filter(|(_, local, _)| !code.contains(self.resolution.local(*local).decl.start))
        {
            given.entry(*local).or_default().push(*origin);
        }
        for (local, origins) in given {
            let values = self.values_in(&delta, local);
            delta.insert(local, self.unknown(&values, origins));
        }
        delta
    }

    /// `values` and those of `origins`, with nothing known about any of them.
    fn unknown(&self, values: &[Version], origins: impl IntoIterator<Item = u32>) -> Values {
        let mut out: Vec<Version> =
            values.iter().map(|value| Version { origin: value.origin, facts: FactList::default() }).collect();
        for origin in origins {
            if !out.iter().any(|value| value.origin == origin) {
                out.push(Version { origin, facts: FactList::default() });
            }
        }
        out.into()
    }

    /// Walks one run of a `while` loop from what the locals hold at its start: what changed since
    /// `mark` where the loop ends, and where it starts again.
    fn while_run(
        &mut self,
        stmt: &'a Stmt,
        cond: &'a Expr,
        body: &'a Block,
        mark: usize,
    ) -> (Vec<Delta>, Option<Delta>) {
        let reached = self.live;
        self.visit_expr(cond);
        let ended = reached && !matches!(cond.unparen().kind, ExprKind::True);
        let ended = ended.then(|| self.assuming(mark, cond, false, stmt.span.end));
        self.assume(cond, true, cond.span.end);
        let mut ways = self.loop_body(stmt, body, mark);
        let again = self.live.then(|| self.delta(mark));
        ways.extend(ended);
        (ways, again)
    }

    /// Walks one run of a `repeat` loop from what the locals hold at its start: what changed since
    /// `mark` where the loop ends, and where it starts again.
    fn repeat_run(
        &mut self,
        stmt: &'a Stmt,
        body: &'a Block,
        cond: &'a Expr,
        mark: usize,
    ) -> (Vec<Delta>, Option<Delta>) {
        self.frame().loops.push((stmt.span.start, mark));
        self.frame().breaks.push(Vec::new());
        self.statements(body);
        let mut ways = self.frame().breaks.pop().unwrap_or_default();
        self.frame().loops.pop();
        // The locals of the body are still there in the condition.
        self.visit_expr(cond);
        self.forget(body);
        let mut again = None;
        if self.live {
            if !matches!(cond.unparen().kind, ExprKind::True) {
                again = Some(self.assuming(mark, cond, false, body.span.start));
            }
            if !matches!(cond.unparen().kind, ExprKind::False | ExprKind::Nil) {
                ways.push(self.assuming(mark, cond, true, stmt.span.end));
            }
        }
        (ways, again)
    }

    fn for_loop(&mut self, stmt: &'a Stmt, body: &'a Block) {
        let (reached, mark) = (self.live, self.mark());
        self.loop_start(stmt, body.span.start, mark, |walker| walker.for_run(stmt, body, mark).1);
        let (ways, _) = self.for_run(stmt, body, mark);
        self.rollback(mark, Some(stmt.span.end));
        self.leave(stmt.span.end, ways);
        self.live &= reached;
    }

    /// Walks one run of a `for` loop from what the locals hold at its start: what changed since
    /// `mark` where the loop ends, and where it starts again.
    fn for_run(&mut self, stmt: &'a Stmt, body: &'a Block, mark: usize) -> (Vec<Delta>, Option<Delta>) {
        let start = self.within(stmt, self.delta(mark));
        let mut ways = self.loop_body(stmt, body, mark);
        // It ends where a run would start, when what it goes through runs out.
        ways.push(start);
        (ways, self.live.then(|| self.delta(mark)))
    }

    /// Walks the body of the loop `stmt`, giving what changed since `mark` at its `break`s.
    fn loop_body(&mut self, stmt: &Stmt, body: &'a Block, mark: usize) -> Vec<Delta> {
        self.frame().loops.push((stmt.span.start, mark));
        self.frame().breaks.push(Vec::new());
        self.block(body);
        let ways = self.frame().breaks.pop().unwrap_or_default();
        self.frame().loops.pop();
        ways
    }

    fn goto(&mut self, stmt: &Stmt, name: &Name) {
        if !self.live {
            return;
        }
        let labels = &self.frames.last().expect("the main chunk is always walked").labels;
        let target = labels.iter().rev().find_map(|(labels, mark)| {
            labels.iter().find(|(label, _)| *label == name.text).map(|(_, at)| (*at, *mark))
        });
        // A `goto` back to a label is handled at the label.
        if let Some((at, mark)) = target.filter(|(at, _)| *at > stmt.span.start) {
            let delta = self.delta(mark);
            self.gotos.entry(at).or_default().push(delta);
        }
    }

    /// Goes on at a label from the code before it and the `goto`s that jump ahead to it. A `goto`
    /// after it may jump back to it, where the locals assigned since may hold any value given there.
    fn label(&mut self, stmt: &Stmt, name: &Name, block: &Block, index: usize) {
        let mark = self.frames.last().and_then(|frame| frame.labels.last()).map_or(0, |(_, mark)| *mark);
        let jumps = self.gotos.remove(&stmt.span.start).unwrap_or_default();
        let ways: Vec<Delta> = self.live.then(|| self.delta(mark)).into_iter().chain(jumps).collect();
        if ways.is_empty() {
            return;
        }
        self.rollback(mark, Some(stmt.span.start));
        let mut joined = self.joined_ways(&ways);
        let mut jumps = Jumps { name: &name.text, found: false };
        block.stmts[index + 1..].iter().for_each(|stmt| jumps.visit_stmt(stmt));
        if jumps.found {
            joined = self.widened(Span::new(stmt.span.start, block.span.end), joined);
            let fields: Vec<LocalId> =
                self.fields.iter().copied().chain(joined.keys().copied().filter(|id| self.is_path(*id))).collect();
            for field in fields {
                joined.insert(field, self.flow.declared.clone());
            }
        }
        self.apply(stmt.span.start, &joined);
        self.live = true;
    }

    /// Walks a function from what the locals around it hold where it is created. A local declared
    /// outside it that is assigned again may also hold, as the function runs any time later, what
    /// the code around it assigns from there on and what other functions assign. Back in the code
    /// that creates it, the locals it assigns may hold what it gives them from then on.
    fn function(&mut self, func: &'a FuncBody) {
        let Some(id) = self.functions.get(&func.span.start).copied() else {
            return self.block(&func.body);
        };
        let (mark, live) = (self.mark(), self.live);
        // While a loop is walked to find what its locals hold at its start, nothing a function holds
        // changes that.
        if self.record {
            self.walk_function(id, func);
        }
        self.rollback(mark, Some(func.span.end));
        // A function that a runtime function is given to run later, as `SetTimeout` is, gives the
        // locals nothing before the code that creates it yields.
        if !self.deferred.contains(&func.span.start) {
            for local in self.inner_writes.get(&id).cloned().unwrap_or_default() {
                if let Some(others) = self.others(local, u32::MAX) {
                    let values = self.with_others(&self.values(local), others);
                    self.set(local, func.span.end, values);
                }
            }
        }
        self.live = live;
    }

    fn walk_function(&mut self, id: FuncId, func: &'a FuncBody) {
        let frame = self.frame();
        frame.created = frame.loops.first().map_or(func.span.start, |(start, _)| (*start).min(func.span.start));
        let at = func.params_span.end;
        let mut others = self.others_of(id);
        for local in self.captured.get(&id).cloned().unwrap_or_default() {
            let decl = self.resolution.local(local).func;
            let created = self.frames.iter().rev().find(|frame| frame.func == decl).map_or(0, |frame| frame.created);
            if let Some(origin) = self.others(local, created) {
                let values = self.with_others(&self.values(local), origin);
                self.set(local, at, values);
                others.push((local, origin));
            }
        }
        // It runs later, when its fields may hold anything.
        let fields: Vec<LocalId> = self.fields.iter().copied().collect();
        for field in fields {
            self.set(field, at, self.flow.declared.clone());
        }
        self.frames.push(Frame { others, ..Frame::new(id) });
        self.live = true;
        self.block(&func.body);
        self.frames.pop();
    }

    /// The local `expr` names.
    fn local(&self, expr: &Expr) -> Option<LocalId> {
        let ExprKind::Name(name) = &expr.unparen().kind else { return None };
        let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.span.start) else { return None };
        Some(id)
    }

    /// The local that the fields, indexes and calls of `expr` are read from, as `data` in
    /// `data?.job.name` and `data:get()`. A value read through it means that it holds one.
    fn read_from(&self, expr: &Expr) -> Option<LocalId> {
        let expr = expr.unparen();
        let base = match &expr.kind {
            ExprKind::Field { base, .. } | ExprKind::Index { base, .. } | ExprKind::MethodCall { base, .. } => base,
            ExprKind::Call { callee, .. } if !matches!(callee.kind, ExprKind::Name(_)) => callee,
            _ => return None,
        };
        match &base.unparen().kind {
            ExprKind::Name(_) => self.local(base),
            _ => self.read_from(base),
        }
    }

    /// Adds that the local and fields that `expr` reads a field, index or call from hold a value, as
    /// `data` and `data.job` do in `data.job.name` and `data.job:get()`.
    fn reads(&self, expr: &Expr, out: &mut Facts) {
        if let Some(local) = self.read_from(expr) {
            out.push((local, Fact::Truthy));
        }
        let mut expr = expr.unparen();
        loop {
            let base = match &expr.kind {
                ExprKind::Field { base, .. } | ExprKind::Index { base, .. } | ExprKind::MethodCall { base, .. } => base,
                ExprKind::Call { callee, .. } if !matches!(callee.kind, ExprKind::Name(_)) => callee,
                _ => return,
            };
            expr = base.unparen();
            if !matches!(expr.kind, ExprKind::Name(_)) && field_path(self.resolution, expr).is_some() {
                out.extend(self.place(expr).map(|field| (field, Fact::Truthy)));
            }
        }
    }

    fn is_reassigned(&self, local: LocalId) -> bool {
        self.writes.contains_key(&local)
    }

    /// Whether a function other than the one being walked assigns `local`, which it may do at any
    /// time.
    fn assigned_elsewhere(&self, local: LocalId) -> bool {
        let func = self.frames.last().map_or(MAIN_CHUNK, |frame| frame.func);
        let writes = self.writes.get(&local).map_or(&[][..], Vec::as_slice);
        writes.iter().any(|write| write.func != func)
    }

    fn is_global(&self, name: &Name) -> bool {
        !matches!(self.resolution.resolve_at(name.span.start), Some(Resolved::Local(_)))
    }

    /// The local that a `type(value)` call checks, or with `true` a `math.type(value)` call.
    fn type_call(&self, expr: &Expr) -> Option<(LocalId, bool)> {
        let ExprKind::Call { callee, args, .. } = &expr.unparen().kind else { return None };
        let [arg] = args.as_slice() else { return None };
        let numbers = self.type_function(callee)?;
        Some((self.place(arg)?, numbers))
    }

    /// Whether `expr` is the global `type` (`false`) or `math.type` (`true`), or a local that holds
    /// one of them, as after `local type = type`.
    fn type_function(&self, expr: &Expr) -> Option<bool> {
        match &expr.kind {
            ExprKind::Name(name) => match self.resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => self.type_functions.get(&id).copied(),
                _ => (name.text == "type").then_some(false),
            },
            ExprKind::Field { base, name, .. } if name.text == "type" => match &base.kind {
                ExprKind::Name(math) if math.text == "math" && self.is_global(math) => Some(true),
                _ => None,
            },
            _ => None,
        }
    }

    /// The local whose kind `side` gives and the name of a kind that `other` compares it with, as
    /// in `type(value) == 'table'`, `math.type(value) == 'integer'` and, after
    /// `local kind = type(value)`, `kind == 'table'`.
    fn kind_check(&self, side: &Expr, other: &Expr) -> Option<(LocalId, &'static str)> {
        let name = other.unparen().as_string()?;
        let (local, numbers) = match &side.unparen().kind {
            ExprKind::Name(_) => {
                let (local, numbers, held) = self.kinds.get(&self.local(side)?)?;
                // Assigned since, the local holds another value than the one whose kind it tells.
                if held.as_ref().is_some_and(|held| *held != origins(&self.values(*local))) {
                    return None;
                }
                (*local, *numbers)
            }
            _ => self.type_call(side)?,
        };
        let names: &[&'static str] = if numbers { &NUMBER_NAMES } else { &TYPE_NAMES };
        names.iter().find(|kind| **kind == name.as_str()).map(|kind| (local, *kind))
    }

    /// Adds what `cond` being true, or false without `holds`, tells about the locals in it.
    fn facts(&self, cond: &Expr, holds: bool, out: &mut Facts) {
        let cond = cond.unparen();
        match &cond.kind {
            ExprKind::Name(_) => {
                if let Some(local) = self.local(cond) {
                    out.push((local, if holds { Fact::Truthy } else { Fact::Falsy }));
                    self.aliased(local, holds, out);
                }
            }
            ExprKind::Field { .. } | ExprKind::Index { .. } if field_path(self.resolution, cond).is_some() => {
                out.extend(self.place(cond).map(|field| (field, if holds { Fact::Truthy } else { Fact::Falsy })));
                if holds {
                    self.reads(cond, out);
                }
            }
            ExprKind::Unary { op: UnOp::Not, expr } => self.facts(expr, !holds, out),
            // Both sides of a true `and` are true, and both sides of a false `or` are false.
            ExprKind::Binary { op: BinOp::And, lhs, rhs, .. } if holds => {
                self.facts(lhs, true, out);
                self.facts(rhs, true, out);
            }
            ExprKind::Binary { op: BinOp::Or, lhs, rhs, .. } if !holds => {
                self.facts(lhs, false, out);
                self.facts(rhs, false, out);
            }
            // One side of a true `or` is true, and one side of a false `and` is false: the other one
            // when a side never is, as the `nil` of `name and value or nil` never is true.
            ExprKind::Binary { op: BinOp::Or | BinOp::And, lhs, rhs, .. } => {
                let never = |side: &Expr| if holds { never_true(side) } else { never_false(side) };
                match (never(lhs), never(rhs)) {
                    (true, false) => self.facts(rhs, holds, out),
                    (false, true) => self.facts(lhs, holds, out),
                    _ => self.either(lhs, rhs, holds, out),
                }
            }
            ExprKind::Binary { op: op @ (BinOp::Eq | BinOp::Ne), lhs, rhs, .. } => {
                let equal = (*op == BinOp::Eq) == holds;
                let compared = match (literal(rhs), literal(lhs)) {
                    (Some(value), _) => self.place(lhs).zip(Some(value)),
                    (None, Some(value)) => self.place(rhs).zip(Some(value)),
                    (None, None) => None,
                };
                if let Some((local, value)) = compared {
                    out.push((local, if equal { Fact::Is(value) } else { Fact::IsNot(value) }));
                } else {
                    // `cam == activeCam` tells about `cam` what the type of `activeCam` there rules
                    // out, and the other way around.
                    for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                        if let (Some(place), Some(other_local)) = (self.place(side), self.local(other)) {
                            let at = other.span.start;
                            out.push((
                                place,
                                match equal {
                                    true => Fact::SameAs { other: other_local, at },
                                    false => Fact::NotSameAs { other: other_local, at },
                                },
                            ));
                        }
                    }
                }
                if let Some((local, kind)) = self.kind_check(lhs, rhs).or_else(|| self.kind_check(rhs, lhs)) {
                    out.push((local, if equal { Fact::Kind(kind) } else { Fact::NotKind(kind) }));
                }
                // `data?.job == 'police'` and `data?.job ~= nil` read a value from `data`.
                for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                    if literal(other).is_some_and(|value| equal != (value == Type::Nil)) {
                        self.reads(side, out);
                    }
                }
            }
            // `data?.job`, `data.job`, `data[key]` and `data:get()` are only true when `data` holds a
            // value: `?.` gives `nil` for a `nil` one, and the others raise an error.
            _ if holds => self.reads(cond, out),
            _ => {}
        }
    }

    /// Adds what `local` being true, or false without `holds`, tells about the locals of the value it
    /// is declared with, when it is never assigned again: after
    /// `local playerName = playerSource and GetPlayerName(playerSource)`, `if playerName then` tells
    /// that `playerSource` holds a value, as TypeScript reads an aliased condition. Only what it tells
    /// about locals that are never assigned again counts, as those hold there what they held where
    /// `local` was declared.
    fn aliased(&self, local: LocalId, holds: bool, out: &mut Facts) {
        let Some(value) = self.conditions.get(&local) else { return };
        let depth = self.alias_depth.get();
        if depth >= MAX_ALIAS_DEPTH {
            return;
        }
        self.alias_depth.set(depth + 1);
        let mut facts = Facts::new();
        self.facts(value, holds, &mut facts);
        self.alias_depth.set(depth);
        out.extend(facts.into_iter().filter(|(of, fact)| self.is_constant(*of) && self.stands_alone(fact)));
    }

    /// Whether `id` is a local that is never assigned again after its declaration, which a field is
    /// not.
    fn is_constant(&self, id: LocalId) -> bool {
        !self.is_path(id) && self.resolution.local(id).refs.iter().all(|reference| !reference.write)
    }

    /// Whether `fact` tells the same wherever it is read: one that compares with another local, as
    /// `cam == activeCam` does, only when that local is never assigned again either.
    fn stands_alone(&self, fact: &Fact) -> bool {
        match fact {
            Fact::SameAs { other, .. } | Fact::NotSameAs { other, .. } => self.is_constant(*other),
            Fact::AnyOf(lists) => lists.iter().flatten().all(|fact| self.stands_alone(fact)),
            _ => true,
        }
    }

    /// Adds what one of `lhs` and `rhs` being true, or false without `holds`, tells: for each local
    /// that both tell something about, that the facts of one of them hold.
    fn either(&self, lhs: &Expr, rhs: &Expr, holds: bool, out: &mut Facts) {
        let (mut left, mut right) = (Facts::new(), Facts::new());
        self.facts(lhs, holds, &mut left);
        self.facts(rhs, holds, &mut right);
        let about = |facts: &Facts, local: LocalId| -> Vec<Fact> {
            facts.iter().filter(|(of, _)| *of == local).map(|(_, fact)| fact.clone()).collect()
        };
        let mut locals: Vec<LocalId> = Vec::new();
        for (local, _) in &left {
            if !locals.contains(local) {
                locals.push(*local);
            }
        }
        for local in locals {
            let second = about(&right, local);
            if !second.is_empty() {
                out.push((local, Fact::AnyOf(vec![about(&left, local), second])));
            }
        }
    }

    /// Links the locals that take the values of the call ending a `local` statement or an
    /// assignment, each named with the origin of the value it takes. A guard on one only tells about
    /// the others while they hold those values, see `Flow::facts`.
    fn link(&mut self, names: &[Option<(&Name, u32)>], exprs: &[Expr]) {
        let Some(first) = exprs.len().checked_sub(1).filter(|_| exprs.last().is_some_and(Expr::is_call)) else {
            return;
        };
        let members: Vec<(LocalId, u32, usize)> = names
            .iter()
            .enumerate()
            .skip(first)
            .filter_map(|(index, name)| {
                let (name, origin) = (*name)?;
                let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.span.start) else { return None };
                Some((id, origin, index - first))
            })
            .collect();
        let linked = |(local, origin, _): &(LocalId, u32, usize)| self.flow.link_of.contains_key(&(*local, *origin));
        if members.len() > 1 && !members.iter().any(linked) {
            for (local, origin, _) in &members {
                self.flow.link_of.insert((*local, *origin), self.flow.links.len());
            }
            self.flow.links.push(members);
        }
    }
}

impl<'a> Visitor<'a> for Walker<'a, '_> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match &expr.kind {
            ExprKind::Function(func) => self.function(func),
            // `b` of `a and b` only runs when `a` is true, and `b` of `a or b` when it is false.
            ExprKind::Binary { op: op @ (BinOp::And | BinOp::Or), lhs, rhs, .. } => {
                self.visit_expr(lhs);
                let mark = self.mark();
                self.assume(lhs, *op == BinOp::And, rhs.span.start);
                self.visit_expr(rhs);
                let after = self.delta(mark);
                self.rollback(mark, Some(rhs.span.end));
                let joined = self.joined_ways(&[Delta::default(), after]);
                self.apply(rhs.span.end, &joined);
            }
            // What a call is given, it may change the fields of, and one that yields lets other code
            // run.
            ExprKind::Call { args, .. } => {
                visit::walk_expr(self, expr);
                args.iter().for_each(|arg| self.forget_fields_of(arg, expr.span.end));
                self.call_ended(expr);
            }
            ExprKind::MethodCall { base, args, .. } => {
                visit::walk_expr(self, expr);
                for given in std::iter::once(&**base).chain(args) {
                    self.forget_fields_of(given, expr.span.end);
                }
                self.call_ended(expr);
            }
            _ => visit::walk_expr(self, expr),
        }
    }
}

/// Whether a call of the function at `path` yields, and so lets other code run, as the `fivem` rules
/// read it: `Wait`, `Citizen.Wait`, `Citizen.Await` and `coroutine.yield`, and those read as
/// `await`, such as `lib.callback.await` and `MySQL.query.await`.
fn is_yield_path(path: &str) -> bool {
    let last = path.rsplit('.').next().unwrap_or(path);
    matches!(path, "Wait" | "Citizen.Wait" | "Citizen.Await" | "coroutine.yield") || last.eq_ignore_ascii_case("await")
}

/// Whether the runtime function at `path` runs the functions it is given later, once the code that
/// calls it yields, as a timeout, a thread or a handler does.
fn defers_path(path: &str) -> bool {
    matches!(
        path,
        "SetTimeout"
            | "Citizen.SetTimeout"
            | "CreateThread"
            | "Citizen.CreateThread"
            | "AddEventHandler"
            | "RegisterNetEvent"
            | "RegisterServerEvent"
            | "RegisterCommand"
            | "RegisterNUICallback"
            | "AddStateBagChangeHandler"
            | "exports"
    )
}

/// Finds the functions written as arguments of the calls of runtime functions that run them later,
/// as `defers_path` picks them.
struct Deferred<'r> {
    resolution: &'r Resolution,
    out: FxHashSet<u32>,
}

impl<'ast> Visitor<'ast> for Deferred<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let ExprKind::Call { callee, args, .. } = &expr.kind {
            let mut root = callee.unparen();
            while let ExprKind::Field { base, .. } = &root.kind {
                root = base.unparen();
            }
            let global = matches!(&root.kind, ExprKind::Name(name)
                if matches!(self.resolution.resolve_at(name.span.start), Some(Resolved::Global(_))));
            if global && callee.dotted_path().as_deref().is_some_and(defers_path) {
                for arg in args {
                    if let ExprKind::Function(func) = &arg.unparen().kind {
                        self.out.insert(func.span.start);
                    }
                }
            }
        }
        visit::walk_expr(self, expr);
    }
}

/// What a call calls, as far as telling whether it may yield goes.
enum Callee {
    Yields,
    Local(LocalId),
    Path(String),
    Other,
}

/// Finds the calls of a file that may yield: those `is_yield_path` picks, and those of the functions
/// of the file that make such a call.
struct Yields<'r> {
    resolution: &'r Resolution,
    functions: &'r FxHashMap<u32, FuncId>,
    /// The function being visited.
    func: FuncId,
    /// The functions of the file, by the local they are given to.
    locals: FxHashMap<LocalId, Vec<FuncId>>,
    /// The functions of the file, by the path of the global or field they are given to.
    paths: FxHashMap<String, Vec<FuncId>>,
    /// Each call, with where it starts, the function it is in, and what it calls.
    calls: Vec<(u32, FuncId, Callee)>,
}

impl<'r> Yields<'r> {
    /// The calls of `chunk` that may yield, by where they start, in order, each with the function it
    /// is in.
    fn of(chunk: &Chunk, resolution: &'r Resolution, functions: &'r FxHashMap<u32, FuncId>) -> Vec<(u32, FuncId)> {
        let mut finder = Yields {
            resolution,
            functions,
            func: MAIN_CHUNK,
            locals: FxHashMap::default(),
            paths: FxHashMap::default(),
            calls: Vec::new(),
        };
        finder.visit_block(&chunk.block);
        let mut yielding: FxHashSet<FuncId> = FxHashSet::default();
        loop {
            let found = yielding.len();
            for (_, func, callee) in &finder.calls {
                if finder.yields(callee, &yielding) {
                    yielding.insert(*func);
                }
            }
            if yielding.len() == found {
                break;
            }
        }
        let calls = finder.calls.iter().filter(|(_, _, callee)| finder.yields(callee, &yielding));
        let mut calls: Vec<(u32, FuncId)> = calls.map(|(at, func, _)| (*at, *func)).collect();
        calls.sort_unstable();
        calls
    }

    /// Whether a call of `callee` may yield, when the functions of the file in `yielding` do.
    fn yields(&self, callee: &Callee, yielding: &FxHashSet<FuncId>) -> bool {
        let funcs = match callee {
            Callee::Yields => return true,
            Callee::Local(local) => self.locals.get(local),
            Callee::Path(path) => self.paths.get(path),
            Callee::Other => None,
        };
        funcs.is_some_and(|funcs| funcs.iter().any(|func| yielding.contains(func)))
    }

    fn local(&self, name: &Name) -> Option<LocalId> {
        match self.resolution.resolve_at(name.span.start) {
            Some(Resolved::Local(local)) => Some(local),
            _ => None,
        }
    }

    /// The local that `expr` names.
    fn local_of(&self, expr: &Expr) -> Option<LocalId> {
        match &expr.unparen().kind {
            ExprKind::Name(name) => self.local(name),
            _ => None,
        }
    }

    /// Notes that the local `local`, or else the global or field at `path`, is given `value`, when
    /// that is a function.
    fn given(&mut self, local: Option<LocalId>, path: Option<String>, value: &Expr) {
        let ExprKind::Function(func) = &value.unparen().kind else { return };
        let Some(func) = self.functions.get(&func.span.start).copied() else { return };
        match (local, path) {
            (Some(local), _) => self.locals.entry(local).or_default().push(func),
            (None, Some(path)) => self.paths.entry(path).or_default().push(func),
            (None, None) => {}
        }
    }

    fn callee(&self, callee: &Expr) -> Callee {
        let callee = callee.unparen();
        let path = callee.dotted_path();
        let awaits = matches!(&callee.kind, ExprKind::Field { name, .. } if name.text.eq_ignore_ascii_case("await"));
        if awaits || path.as_deref().is_some_and(is_yield_path) {
            return Callee::Yields;
        }
        match self.local_of(callee) {
            Some(local) => Callee::Local(local),
            None => path.map_or(Callee::Other, Callee::Path),
        }
    }
}

impl<'ast> Visitor<'ast> for Yields<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::LocalFunction { name, func } => {
                if let (Some(local), Some(func)) = (self.local(name), self.functions.get(&func.span.start)) {
                    self.locals.entry(local).or_default().push(*func);
                }
            }
            StmtKind::Local { names, exprs, .. } => {
                for (name, value) in names.iter().zip(exprs) {
                    self.given(self.local(&name.name), None, value);
                }
            }
            StmtKind::Function { name, func } => {
                if let Some(func) = self.functions.get(&func.span.start).copied() {
                    match self.local(&name.base).filter(|_| name.path.is_empty() && name.method.is_none()) {
                        Some(local) => self.locals.entry(local).or_default().push(func),
                        None => {
                            let names = std::iter::once(&name.base).chain(&name.path).chain(&name.method);
                            let path: Vec<&str> = names.map(|name| name.text.as_str()).collect();
                            self.paths.entry(path.join(".")).or_default().push(func);
                        }
                    }
                }
            }
            StmtKind::Assign { targets, exprs } => {
                for (target, value) in targets.iter().zip(exprs) {
                    self.given(self.local_of(target), target.dotted_path(), value);
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        let callee = match &expr.kind {
            ExprKind::Call { callee, .. } => Some(self.callee(callee)),
            ExprKind::MethodCall { base, method, .. } => Some(match method.text.eq_ignore_ascii_case("await") {
                true => Callee::Yields,
                false => {
                    base.dotted_path().map_or(Callee::Other, |path| Callee::Path(format!("{path}.{}", method.text)))
                }
            }),
            _ => None,
        };
        if let Some(callee) = callee {
            self.calls.push((expr.span.start, self.func, callee));
        }
        visit::walk_expr(self, expr);
    }

    fn visit_func_body(&mut self, func: &'ast FuncBody) {
        let outer = self.func;
        self.func = self.functions.get(&func.span.start).copied().unwrap_or(outer);
        visit::walk_func_body(self, func);
        self.func = outer;
    }
}

/// Adds the tables that `expr` names or reads to `out`, each as a local or global and the keys after
/// it, with those that the locals it reads from were read from, and for a table it builds, those of
/// the values in it.
fn tables_of(resolution: &Resolution, aliases: &Aliases, expr: &Expr, out: &mut Vec<(Root, Vec<SmolStr>)>) {
    let expr = expr.unparen();
    if let ExprKind::Table(fields) = &expr.kind {
        for field in fields {
            match field {
                TableField::Positional(value) | TableField::Named { value, .. } | TableField::Keyed { value, .. } => {
                    tables_of(resolution, aliases, value, out);
                }
                TableField::SetMember(_) => {}
            }
        }
        return;
    }
    let table = match &expr.kind {
        ExprKind::Name(name) => Some((name_root(resolution, name), Vec::new())),
        _ => field_path(resolution, expr),
    };
    let Some((root, keys)) = table else { return };
    if let Root::Local(local) = &root {
        for (alias, prefix) in aliases.get(local).into_iter().flatten() {
            out.push((alias.clone(), prefix.iter().chain(&keys).cloned().collect()));
        }
    }
    out.push((root, keys));
}

/// What the code of a loop may change before it starts again: the locals, globals and the fields of
/// each name it assigns, and the fields of the tables it gives calls or assigns keys of that are not
/// known.
struct Changes<'r> {
    resolution: &'r Resolution,
    aliases: &'r Aliases,
    locals: Vec<LocalId>,
    globals: Vec<SmolStr>,
    keys: Vec<SmolStr>,
    tables: Vec<(Root, Vec<SmolStr>)>,
}

impl Changes<'_> {
    fn changes(&self, root: &Root, keys: &[SmolStr]) -> bool {
        let assigned = match root {
            Root::Local(local) => self.locals.contains(local),
            Root::Global(name) => self.globals.contains(name),
        };
        assigned
            || keys.iter().any(|key| self.keys.contains(key))
            || self
                .tables
                .iter()
                .any(|(table, prefix)| table == root && keys.len() > prefix.len() && keys.starts_with(prefix))
    }

    /// Notes that the fields of the tables `expr` names or reads may change.
    fn table(&mut self, expr: &Expr) {
        tables_of(self.resolution, self.aliases, expr, &mut self.tables);
    }

    fn assigned(&mut self, target: &Expr) {
        match &target.unparen().kind {
            ExprKind::Name(name) => match self.resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => self.locals.push(id),
                _ => self.globals.push(name.text.clone()),
            },
            ExprKind::Field { name, .. } => self.keys.push(name.text.clone()),
            ExprKind::Index { base, index, .. } => match index.as_string() {
                Some(key) => self.keys.push(SmolStr::new(key)),
                None => self.table(base),
            },
            _ => {}
        }
    }
}

impl<'ast> Visitor<'ast> for Changes<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::Assign { targets, .. } => targets.iter().for_each(|target| self.assigned(target)),
            StmtKind::CompoundAssign { target, .. } => self.assigned(target),
            StmtKind::Function { name, .. } => match name.method.as_ref().or(name.path.last()) {
                Some(key) => self.keys.push(key.text.clone()),
                None => self.assigned(&Expr { kind: ExprKind::Name(name.base.clone()), span: name.base.span }),
            },
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Call { args, .. } => args.iter().for_each(|arg| self.table(arg)),
            ExprKind::MethodCall { base, args, .. } => {
                self.table(base);
                args.iter().for_each(|arg| self.table(arg));
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }

    // A function in the loop changes nothing until it is called.
    fn visit_func_body(&mut self, _: &'ast FuncBody) {}
}

/// The local or global that `expr` reads a field from and the keys it reads through, as `data` and
/// `job`, `name` for `data.job.name` or `data["job"].name`, for at most `MAX_KEYS` keys written out.
pub fn field_path(resolution: &Resolution, expr: &Expr) -> Option<(Root, Vec<SmolStr>)> {
    let mut keys = Vec::new();
    let mut expr = expr.unparen();
    loop {
        match &expr.kind {
            ExprKind::Field { base, name, .. } => {
                keys.push(name.text.clone());
                expr = base.unparen();
            }
            ExprKind::Index { base, index, .. } => {
                keys.push(SmolStr::new(index.as_string()?));
                expr = base.unparen();
            }
            ExprKind::Name(name) if !keys.is_empty() && keys.len() <= MAX_KEYS => {
                keys.reverse();
                return Some((name_root(resolution, name), keys));
            }
            _ => return None,
        }
    }
}

/// Where each of `values` comes from.
fn origins(values: &Values) -> Vec<u32> {
    values.iter().map(|value| value.origin).collect()
}

/// What a local may hold where two ways meet that leave it `a` and `b`: the values of both, each
/// with the facts that hold on both ways.
fn joined(a: &Values, b: &Values) -> Values {
    if same(a, b) {
        return a.clone();
    }
    let mut out: Vec<Version> = a
        .iter()
        .map(|value| match b.iter().find(|other| other.origin == value.origin) {
            Some(other) => Version { origin: value.origin, facts: shared(&value.facts, &other.facts) },
            None => value.clone(),
        })
        .collect();
    out.extend(b.iter().filter(|other| !a.iter().any(|value| value.origin == other.origin)).cloned());
    out.into()
}

/// The facts of `a` that `b` has too, each from where it starts to hold in `a`, so that the start of
/// a loop settles once the facts its runs keep stop changing.
fn shared(a: &FactList, b: &FactList) -> FactList {
    if a.is(b) {
        return a.clone();
    }
    // What both were given before they parted.
    let (mut common, mut other) = (a, b);
    while common.len() > other.len() {
        common = common.before();
    }
    while other.len() > common.len() {
        other = other.before();
    }
    while !common.is(other) {
        (common, other) = (common.before(), other.before());
    }
    let added: Vec<&FactNode> = a.latest().take(a.len() - common.len()).collect();
    let kept: Vec<&FactNode> =
        added.iter().copied().filter(|node| b.latest().any(|other| other.fact == node.fact)).collect();
    if kept.len() == added.len() {
        return a.clone();
    }
    kept.iter().rev().fold(common.clone(), |list, node| list.with(node.at, node.fact.clone()))
}

/// Whether `a` and `b` hold the same values.
fn same(a: &Values, b: &Values) -> bool {
    Rc::ptr_eq(a, b) || a == b
}

/// Whether `a` and `b` tell the same changes.
fn same_deltas(a: &Delta, b: &Delta) -> bool {
    a.len() == b.len() && a.iter().all(|(local, values)| b.get(local).is_some_and(|other| same(values, other)))
}

/// Finds a `goto` to a label in the code after it, which jumps back to it.
struct Jumps<'n> {
    name: &'n str,
    found: bool,
}

impl<'ast> Visitor<'ast> for Jumps<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::Goto(name) if name.text == self.name => self.found = true,
            _ => visit::walk_stmt(self, stmt),
        }
    }

    // A `goto` cannot leave its function.
    fn visit_func_body(&mut self, _: &'ast FuncBody) {}
}

fn literal(expr: &Expr) -> Option<Type> {
    match &expr.unparen().kind {
        ExprKind::Nil => Some(Type::Nil),
        ExprKind::True => Some(Type::BooleanLit(true)),
        ExprKind::False => Some(Type::BooleanLit(false)),
        ExprKind::String(value) => Some(Type::StringLit(value.clone())),
        ExprKind::Number(NumberValue::Int(value)) => Some(Type::IntLit(*value)),
        ExprKind::Unary { op: UnOp::Neg, expr } => match &expr.unparen().kind {
            ExprKind::Number(NumberValue::Int(value)) => value.checked_neg().map(Type::IntLit),
            _ => None,
        },
        _ => None,
    }
}

/// A `---@cast` line.
#[derive(Debug)]
pub struct Cast {
    /// The local it names, when one by that name is in scope.
    pub local: Option<LocalId>,
    /// What each entry does, with where its type is written.
    pub entries: Vec<(CastEntry, Span)>,
    /// The comment it is written in.
    pub span: Span,
    /// Where the token after it starts: that of the statement it gives the local its value before,
    /// or the `end`, `else`, `elseif` or `until` of the block it ends.
    pub before: u32,
}

impl Cast {
    /// Whether it gives its local a type of its own, rather than adding types to or taking them out
    /// of the one the local has there: an entry that names a type replaces what the ones before it
    /// leave.
    pub fn replaces(&self) -> bool {
        self.entries.iter().any(|(entry, _)| matches!(entry, CastEntry::Replace(_)))
    }
}

/// The `---@cast` lines of a file.
#[derive(Debug, Default)]
pub struct Casts {
    casts: Vec<Cast>,
}

impl Casts {
    pub fn of(source: &str, chunk: &Chunk, resolution: &Resolution) -> Self {
        let tokens = &chunk.tokens;
        let mut casts = Vec::new();
        for comment in chunk.comments.iter().filter(|comment| comment.kind == CommentKind::Line) {
            let text = comment.content.text(source);
            let Some(line) = text.strip_prefix('-').map(str::trim_start) else { continue };
            let Some(cast) = parse_cast(line) else { continue };
            // A line without a type, as while one is being typed, changes nothing.
            if cast.entries.is_empty() {
                continue;
            }
            let base = comment.content.end - line.len() as u32;
            let entries = cast.entries.into_iter();
            let next = tokens.partition_point(|token| token.span.start < comment.span.end);
            casts.push(Cast {
                local: resolution.lookup_local_at(cast.name, comment.span.start),
                entries: entries
                    .map(|(entry, at)| (entry, Span::new(base + at.start as u32, base + at.end as u32)))
                    .collect(),
                span: comment.span,
                before: tokens.get(next).map_or(u32::MAX, |token| token.span.start),
            });
        }
        Self { casts }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Cast> {
        self.casts.iter()
    }

    pub fn get(&self, index: u32) -> Option<&Cast> {
        self.casts.get(index as usize)
    }
}
