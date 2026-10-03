//! Type guards and the values locals hold: which of the values given to a local reach each point of
//! the code, and what a condition tells about the locals it tests in the code that only runs when it
//! held or failed. `if not name then return end` leaves `name` holding a value for the rest of its
//! block, `if name then ... end` for the branch, and `name = name or 'none'` gives it a new one.
//! `---@cast` lines change the type of a local from their line on.

use std::rc::Rc;

use qbx_lua_analysis::scope::{FuncId, LocalId, Resolution, Resolved, MAIN_CHUNK};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{CommentKind, NumberValue, SmolStr, Span, Token, TokenKind};
use rustc_hash::FxHashMap;

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

/// The index of the declaration of a local among the origins of the values it holds.
pub const DECLARATION: u32 = 0;
/// How many times a loop is walked at most to find what its locals hold at its start.
const MAX_ROUNDS: usize = 4;

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
}

/// One of the values a local may hold at a point of the code.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    /// Where it comes from, as an index into the origins of the file.
    pub origin: u32,
    /// What the guards since then tell about it, each with where it starts to hold.
    pub facts: Rc<[(u32, Fact)]>,
}

/// The values a local may hold.
type Values = Rc<[Version]>;

/// What the locals may hold at a point, for those that may hold more than the value of their
/// declaration with nothing known about it.
type State = FxHashMap<LocalId, Values>;

/// Which of the values given to the locals of a file may reach each point of its code, and what the
/// guards on the way tell about them. A value reaches the code after the statement that assigns it,
/// branches join where they meet, and a loop starts with what its runs before leave. Calls are
/// taken to change no local, but a function runs any time after it is created: inside it, a local
/// may also hold what the code after its creation and the other functions assign, and the code
/// that creates it may find what it assigns from there on.
#[derive(Debug)]
pub struct Flow<'a> {
    origins: Vec<Origin<'a>>,
    /// The origin of the value each assignment gives a local, by the start of the name it assigns.
    written: FxHashMap<u32, u32>,
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
}

impl<'a> Flow<'a> {
    pub fn of(chunk: &'a Chunk, resolution: &Resolution) -> Self {
        let mut origins = Origins { resolution, origins: vec![Origin::Declaration], written: FxHashMap::default() };
        origins.visit_block(&chunk.block);
        let no_facts: Rc<[(u32, Fact)]> = Rc::from([]);
        let flow = Flow {
            origins: origins.origins,
            written: origins.written,
            chained: FxHashMap::default(),
            points: FxHashMap::default(),
            declared: Rc::from([Version { origin: DECLARATION, facts: no_facts.clone() }]),
            links: Vec::new(),
            link_of: FxHashMap::default(),
        };
        let mut walker = Walker::new(resolution, flow, no_facts);
        walker.block(&chunk.block);
        walker.flow
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
        let facts = value.map_or(&[][..], |value| &value.facts[..]);
        facts.iter().filter(move |(at, _)| *at <= code.start).map(|(_, fact)| fact)
    }

    /// The locals that the call which gives `local` the value of `origin` gives values too, itself
    /// included, each with the origin of its value and the position of that value among those the
    /// call returns. A guard on one of them also tells which of the sets of values the call can
    /// return the others come from.
    pub fn linked(&self, local: LocalId, origin: u32) -> Option<&[(LocalId, u32, usize)]> {
        self.link_of.get(&(local, origin)).map(|link| self.links[*link].as_slice())
    }
}

/// Finds the assignments to locals, which are the origins of the values they hold after their
/// declaration.
struct Origins<'a, 'r> {
    resolution: &'r Resolution,
    origins: Vec<Origin<'a>>,
    written: FxHashMap<u32, u32>,
}

impl<'a> Origins<'a, '_> {
    fn add(&mut self, name: &Name, origin: Origin<'a>) {
        if let Some(Resolved::Local(_)) = self.resolution.resolve_at(name.span.start) {
            self.written.insert(name.span.start, self.origins.len() as u32);
            self.origins.push(origin);
        }
    }
}

impl<'a> Visitor<'a> for Origins<'a, '_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match &stmt.kind {
            StmtKind::Assign { targets, .. } => {
                for (index, target) in targets.iter().enumerate() {
                    if let ExprKind::Name(name) = &target.kind {
                        self.add(name, Origin::Assignment { stmt, index });
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

/// A function being walked.
struct Frame {
    func: FuncId,
    /// Where the loops around the code being walked start, outermost first.
    loops: Vec<u32>,
    /// What the locals hold at the `break`s of each of those loops.
    breaks: Vec<Vec<State>>,
    /// The labels of the blocks around the code being walked, innermost last, each with where its
    /// statement starts.
    labels: Vec<Vec<(SmolStr, u32)>>,
    /// Where the function being walked inside this one is created, or the outermost loop around
    /// that starts: the assignments of this function from there on may run before it does.
    created: u32,
}

impl Frame {
    fn new(func: FuncId) -> Self {
        Self { func, loops: Vec::new(), breaks: Vec::new(), labels: Vec::new(), created: 0 }
    }
}

/// Walks the code of a file in the order it runs and records what its locals may hold.
struct Walker<'a, 'r> {
    resolution: &'r Resolution,
    flow: Flow<'a>,
    state: State,
    /// Whether the code being walked can run: not after a `return`, `break`, `goto` or `error()`.
    live: bool,
    /// Whether what the locals hold is recorded, which it is not while a loop is walked to find what
    /// its locals hold at its start.
    record: bool,
    no_facts: Rc<[(u32, Fact)]>,
    frames: Vec<Frame>,
    /// Each function by where it starts.
    functions: FxHashMap<u32, FuncId>,
    /// The assignments to each local that is assigned again.
    writes: FxHashMap<LocalId, Vec<Write>>,
    /// Where each assignment to a local starts, in order, with the local and the origin of the value.
    ordered: Vec<(u32, LocalId, u32)>,
    /// For each function, the locals declared outside it that are assigned again and that it, or a
    /// function in it, reads or assigns.
    captured: FxHashMap<FuncId, Vec<LocalId>>,
    /// For each function, the values that it, or a function in it, gives the locals declared
    /// outside it.
    inner_writes: FxHashMap<FuncId, Vec<(LocalId, u32)>>,
    /// What the locals hold at the `goto`s that jump ahead to a label, by where its statement starts.
    gotos: FxHashMap<u32, Vec<State>>,
    /// The locals that hold what `type`, or `math.type` with `true`, gives for another local, as
    /// `local kind = type(value)` does.
    kinds: FxHashMap<LocalId, (LocalId, bool)>,
    /// The locals that hold `type`, or with `true` `math.type`, as after `local type = type`.
    type_functions: FxHashMap<LocalId, bool>,
}

impl<'a, 'r> Walker<'a, 'r> {
    fn new(resolution: &'r Resolution, flow: Flow<'a>, no_facts: Rc<[(u32, Fact)]>) -> Self {
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
        ordered.sort_unstable();
        // The functions between where a local is used and the function that declares it.
        let outside = |from: FuncId, decl: FuncId| {
            std::iter::successors(Some(from), |func| resolution.functions.get(*func as usize).and_then(|f| f.parent))
                .take_while(move |func| *func != decl)
        };
        let mut captured: FxHashMap<FuncId, Vec<LocalId>> = FxHashMap::default();
        let mut inner_writes: FxHashMap<FuncId, Vec<(LocalId, u32)>> = FxHashMap::default();
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
                    inner_writes.entry(func).or_default().push((*id, write.origin));
                }
            }
        }
        let functions = resolution.functions.iter().enumerate().skip(1);
        let mut flow = flow;
        for (local, writes) in &writes {
            let reads = |stmt: &Stmt| {
                let refs = &resolution.local(*local).refs;
                refs.iter().any(|r| !r.write && stmt.span.contains(r.span.start))
            };
            let chained = writes.iter().filter(|write| match flow.origin(write.origin) {
                Origin::Assignment { stmt, .. } | Origin::Compound { stmt } => reads(stmt),
                _ => false,
            });
            let mut origins: Vec<(u32, u32)> = chained.map(|write| (write.start, write.origin)).collect();
            origins.sort_unstable();
            flow.chained.insert(*local, origins.into_iter().map(|(_, origin)| origin).collect());
        }
        Self {
            resolution,
            flow,
            state: State::default(),
            live: true,
            record: true,
            no_facts,
            frames: vec![Frame::new(MAIN_CHUNK)],
            functions: functions.map(|(id, func)| (func.span.start, id as FuncId)).collect(),
            writes,
            ordered,
            captured,
            inner_writes,
            gotos: FxHashMap::default(),
            kinds: FxHashMap::default(),
            type_functions: FxHashMap::default(),
        }
    }

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("the main chunk is always walked")
    }

    fn is_declared(values: &[Version]) -> bool {
        matches!(values, [value] if value.origin == DECLARATION && value.facts.is_empty())
    }

    fn values(&self, state: &State, local: LocalId) -> Values {
        state.get(&local).cloned().unwrap_or_else(|| self.flow.declared.clone())
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
            Some((_, held)) if held == values => {}
            _ => points.push((at, values.clone())),
        }
    }

    /// Makes `local` hold one of `values` from `at` on.
    fn set(&mut self, local: LocalId, at: u32, values: Values) {
        self.point(local, at, &values);
        if Self::is_declared(&values) {
            self.state.remove(&local);
        } else {
            self.state.insert(local, values);
        }
    }

    /// Makes the locals hold what `state` says from `at` on.
    fn switch(&mut self, at: u32, state: State) {
        if self.record {
            let old = &self.state;
            let changed: Vec<LocalId> = old
                .iter()
                .filter(|(local, values)| state.get(local) != Some(values))
                .map(|(local, _)| *local)
                .chain(state.iter().filter(|(local, values)| old.get(local) != Some(values)).map(|(local, _)| *local))
                .collect();
            for local in changed {
                let values = self.values(&state, local);
                self.point(local, at, &values);
            }
        }
        self.state = state;
    }

    /// `state` with what `facts` tell about its locals from `at` on.
    fn with_facts(&self, mut state: State, facts: Facts, at: u32) -> State {
        for (local, fact) in facts {
            let values = self.values(&state, local);
            let values = values.iter().map(|value| Version {
                origin: value.origin,
                facts: value.facts.iter().cloned().chain([(at, fact.clone())]).collect(),
            });
            state.insert(local, values.collect());
        }
        state
    }

    /// Adds what `cond` being true, or false without `holds`, tells about the locals in it from `at`
    /// on.
    fn assume(&mut self, cond: &Expr, holds: bool, at: u32) {
        let mut facts = Facts::new();
        self.facts(cond, holds, &mut facts);
        if !facts.is_empty() {
            let state = self.with_facts(self.state.clone(), facts, at);
            self.switch(at, state);
        }
    }

    /// `state` with what `cond` being true, or false without `holds`, tells from `at` on.
    fn assuming(&self, state: State, cond: &Expr, holds: bool, at: u32) -> State {
        let mut facts = Facts::new();
        self.facts(cond, holds, &mut facts);
        self.with_facts(state, facts, at)
    }

    /// What the locals may hold where two ways meet that leave them `a` and `b`.
    fn join(&self, a: &State, b: &State) -> State {
        let mut out = State::default();
        let declared = &self.flow.declared;
        for (local, values) in a {
            let joined = joined(values, b.get(local).unwrap_or(declared));
            if !Self::is_declared(&joined) {
                out.insert(*local, joined);
            }
        }
        for (local, values) in b.iter().filter(|(local, _)| !a.contains_key(local)) {
            let joined = joined(declared, values);
            if !Self::is_declared(&joined) {
                out.insert(*local, joined);
            }
        }
        out
    }

    /// Goes on after a statement, from what the ways out of it leave, joined at `at`. With none, the
    /// code after it does not run.
    fn leave(&mut self, at: u32, ways: Vec<State>) {
        let mut ways = ways.into_iter();
        let Some(first) = ways.next() else {
            self.live = false;
            return;
        };
        let joined = ways.fold(first, |joined, way| self.join(&joined, &way));
        self.switch(at, joined);
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
        let labels = labels.collect();
        self.frame().labels.push(labels);
        for (index, stmt) in block.stmts.iter().enumerate() {
            self.stmt(stmt, block, index);
        }
        self.frame().labels.pop();
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
                    self.state.remove(&id);
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
                for name in names {
                    if let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.name.span.start) {
                        if self.state.contains_key(&id) {
                            self.set(id, stmt.span.end, self.flow.declared.clone());
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
                if name.path.is_empty() && name.method.is_none() {
                    self.assigned(&name.base, stmt.span.end);
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
                for target in targets {
                    if let ExprKind::Name(name) = &target.kind {
                        self.assigned(name, stmt.span.end);
                    }
                }
            }
            StmtKind::CompoundAssign { target, expr, .. } => {
                self.visit_expr(expr);
                match &target.kind {
                    ExprKind::Name(name) => self.assigned(name, stmt.span.end),
                    _ => self.visit_expr(target),
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
                let (state, live) = (self.state.clone(), self.live);
                self.block(body);
                self.switch(stmt.span.end, state);
                self.live = live;
            }
            StmtKind::If { branches, else_block } => self.if_stmt(stmt, branches, else_block.as_ref()),
            StmtKind::While { cond, body } => {
                let reached = self.live;
                let start = self.loop_start(stmt, |walker| walker.while_run(stmt, cond, body).1);
                self.switch(cond.span.start, start);
                let (ways, _) = self.while_run(stmt, cond, body);
                self.leave(stmt.span.end, ways);
                self.live &= reached;
            }
            StmtKind::Repeat { body, cond } => {
                let reached = self.live;
                let start = self.loop_start(stmt, |walker| walker.repeat_run(stmt, body, cond).1);
                self.switch(body.span.start, start);
                let (ways, _) = self.repeat_run(stmt, body, cond);
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
                if self.live {
                    let state = self.state.clone();
                    if let Some(breaks) = self.frame().breaks.last_mut() {
                        breaks.push(state);
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
            // `local kind = type(value)` tells nothing about what is assigned to `value` later.
            if let Some(checked) = self.type_call(expr).filter(|(checked, _)| !self.is_reassigned(*checked)) {
                self.kinds.insert(id, checked);
            } else if let Some(numbers) = self.type_function(expr) {
                self.type_functions.insert(id, numbers);
            }
        }
    }

    /// Makes the local whose name `name` assigns hold the value it is given from `at` on.
    fn assigned(&mut self, name: &Name, at: u32) {
        let Some(Resolved::Local(id)) = self.resolution.resolve_at(name.span.start) else { return };
        let Some(origin) = self.flow.written_at(name.span.start) else { return };
        let values: Values = Rc::from([Version { origin, facts: self.no_facts.clone() }]);
        self.set(id, at, values);
    }

    fn if_stmt(&mut self, stmt: &'a Stmt, branches: &'a [IfBranch], else_block: Option<&'a Block>) {
        let reached = self.live;
        let else_start = else_block.map(|block| block.span.start.min(stmt.span.end));
        let mut failed = self.state.clone();
        let mut ways = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            // The conditions before it failed for the condition of an `elseif` too.
            self.switch(branch.keyword_span.end, failed);
            self.live = reached;
            self.visit_expr(&branch.cond);
            let tested = self.state.clone();
            // Measured between the keywords so half-typed code inside the branch still counts.
            self.assume(&branch.cond, true, branch.cond.span.end);
            self.block(&branch.block);
            if self.live {
                ways.push(self.state.clone());
            }
            let next = branches.get(index + 1).map(|branch| branch.keyword_span.end);
            let at = next.or(else_start).unwrap_or(stmt.span.end);
            failed = self.assuming(tested, &branch.cond, false, at);
        }
        match else_block {
            Some(block) => {
                self.switch(else_start.unwrap_or(block.span.start), failed);
                self.live = reached;
                self.block(block);
                if self.live {
                    ways.push(self.state.clone());
                }
            }
            None if reached => ways.push(failed),
            None => {}
        }
        self.leave(stmt.span.end, ways);
    }

    /// What the locals hold at the start of each run of the loop `stmt`, of which `run` walks one
    /// from what they hold at its start, giving what they hold to start it again.
    fn loop_start(&mut self, stmt: &Stmt, run: impl Fn(&mut Self) -> Option<State>) -> State {
        let entry = self.within(stmt, self.state.clone());
        let first = self.ordered.partition_point(|(start, ..)| *start < stmt.span.start);
        if !self.ordered.get(first).is_some_and(|(start, ..)| *start < stmt.span.end) {
            return entry;
        }
        // Inside a loop being walked to find what its locals hold at its start, the loops in it start
        // with any value they give, which keeps loops inside loops from multiplying the walks.
        if !self.record {
            return self.widened(stmt.span, entry);
        }
        let (record, state, live) = (self.record, self.state.clone(), self.live);
        self.record = false;
        let mut start = entry;
        for round in 1..=MAX_ROUNDS {
            self.state = start.clone();
            self.live = true;
            let next = match run(self) {
                Some(again) => self.join(&start, &self.within(stmt, again)),
                None => start.clone(),
            };
            if next == start {
                break;
            }
            start = next;
            if round == MAX_ROUNDS {
                start = self.widened(stmt.span, start);
            }
        }
        (self.record, self.state, self.live) = (record, state, live);
        start
    }

    /// `state` without the locals that the loop `stmt` declares, which each of its runs declares
    /// again.
    fn within(&self, stmt: &Stmt, mut state: State) -> State {
        state.retain(|local, _| !stmt.span.contains(self.resolution.local(*local).decl.start));
        state
    }

    /// `state` where the locals that `code` assigns may hold any value it gives them, with nothing
    /// known about them, as at the start of a loop when walking it again has not settled what they
    /// hold. Those that `code` declares are left out.
    fn widened(&self, code: Span, mut state: State) -> State {
        let first = self.ordered.partition_point(|(start, ..)| *start < code.start);
        let inside = self.ordered[first..].iter().take_while(|(start, ..)| *start < code.end);
        let mut given: FxHashMap<LocalId, Vec<u32>> = FxHashMap::default();
        for (_, local, origin) in
            inside.filter(|(_, local, _)| !code.contains(self.resolution.local(*local).decl.start))
        {
            given.entry(*local).or_default().push(*origin);
        }
        for (local, origins) in given {
            let values = self.values(&state, local);
            state.insert(local, self.unknown(&values, origins));
        }
        state
    }

    /// `values` and those that `origins` may give again, with nothing known about these.
    fn given(&self, values: &[Version], origins: impl IntoIterator<Item = u32>) -> Values {
        let mut out = values.to_vec();
        for origin in origins {
            let fresh = Version { origin, facts: self.no_facts.clone() };
            match out.iter_mut().find(|value| value.origin == origin) {
                Some(value) => *value = fresh,
                None => out.push(fresh),
            }
        }
        out.into()
    }

    /// `values` and those of `origins`, with nothing known about any of them.
    fn unknown(&self, values: &[Version], origins: impl IntoIterator<Item = u32>) -> Values {
        let mut out: Vec<Version> =
            values.iter().map(|value| Version { origin: value.origin, facts: self.no_facts.clone() }).collect();
        for origin in origins {
            if !out.iter().any(|value| value.origin == origin) {
                out.push(Version { origin, facts: self.no_facts.clone() });
            }
        }
        out.into()
    }

    /// Walks one run of a `while` loop from what the locals hold at its start: what they hold where
    /// the loop ends, and where it starts again.
    fn while_run(&mut self, stmt: &'a Stmt, cond: &'a Expr, body: &'a Block) -> (Vec<State>, Option<State>) {
        let reached = self.live;
        self.visit_expr(cond);
        let tested = self.state.clone();
        self.assume(cond, true, cond.span.end);
        let mut ways = self.loop_body(stmt, body);
        let again = self.live.then(|| self.state.clone());
        if reached && !matches!(cond.unparen().kind, ExprKind::True) {
            ways.push(self.assuming(tested, cond, false, stmt.span.end));
        }
        (ways, again)
    }

    /// Walks one run of a `repeat` loop from what the locals hold at its start: what they hold where
    /// the loop ends, and where it starts again.
    fn repeat_run(&mut self, stmt: &'a Stmt, body: &'a Block, cond: &'a Expr) -> (Vec<State>, Option<State>) {
        self.frame().loops.push(stmt.span.start);
        self.frame().breaks.push(Vec::new());
        self.statements(body);
        let mut ways = self.frame().breaks.pop().unwrap_or_default();
        self.frame().loops.pop();
        // The locals of the body are still there in the condition.
        self.visit_expr(cond);
        self.forget(body);
        let tested = self.state.clone();
        let mut again = None;
        if self.live {
            if !matches!(cond.unparen().kind, ExprKind::True) {
                again = Some(self.assuming(tested.clone(), cond, false, body.span.start));
            }
            if !matches!(cond.unparen().kind, ExprKind::False | ExprKind::Nil) {
                ways.push(self.assuming(tested, cond, true, stmt.span.end));
            }
        }
        (ways, again)
    }

    fn for_loop(&mut self, stmt: &'a Stmt, body: &'a Block) {
        let reached = self.live;
        let start = self.loop_start(stmt, |walker| walker.for_run(stmt, body).1);
        self.switch(body.span.start, start);
        let (ways, _) = self.for_run(stmt, body);
        self.leave(stmt.span.end, ways);
        self.live &= reached;
    }

    /// Walks one run of a `for` loop from what the locals hold at its start: what they hold where the
    /// loop ends, and where it starts again.
    fn for_run(&mut self, stmt: &'a Stmt, body: &'a Block) -> (Vec<State>, Option<State>) {
        let start = self.within(stmt, self.state.clone());
        let mut ways = self.loop_body(stmt, body);
        // It ends where a run would start, when what it goes through runs out.
        ways.push(start);
        (ways, self.live.then(|| self.state.clone()))
    }

    /// Walks the body of the loop `stmt`, giving what the locals hold at its `break`s.
    fn loop_body(&mut self, stmt: &Stmt, body: &'a Block) -> Vec<State> {
        self.frame().loops.push(stmt.span.start);
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
        let target = labels.iter().rev().find_map(|labels| labels.iter().find(|(label, _)| *label == name.text));
        // A `goto` back to a label is handled at the label.
        if let Some((_, at)) = target.filter(|(_, at)| *at > stmt.span.start) {
            let state = self.state.clone();
            self.gotos.entry(*at).or_default().push(state);
        }
    }

    /// Goes on at a label from the code before it and the `goto`s that jump ahead to it. A `goto`
    /// after it may jump back to it, where the locals assigned since may hold any value given there.
    fn label(&mut self, stmt: &Stmt, name: &Name, block: &Block, index: usize) {
        let jumps = self.gotos.remove(&stmt.span.start).unwrap_or_default();
        let mut ways = self.live.then(|| self.state.clone()).into_iter().chain(jumps);
        let Some(first) = ways.next() else { return };
        let mut joined = ways.fold(first, |joined, way| self.join(&joined, &way));
        let mut jumps = Jumps { name: &name.text, found: false };
        block.stmts[index + 1..].iter().for_each(|stmt| jumps.visit_stmt(stmt));
        if jumps.found {
            joined = self.widened(Span::new(stmt.span.start, block.span.end), joined);
        }
        self.switch(stmt.span.start, joined);
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
        let (outer, live) = (self.state.clone(), self.live);
        // While a loop is walked to find what its locals hold at its start, nothing a function holds
        // changes that.
        if self.record {
            self.walk_function(id, func);
        }
        let mut after = outer;
        for (local, origin) in self.inner_writes.get(&id).into_iter().flatten() {
            let values = self.values(&after, *local);
            after.insert(*local, self.given(&values, [*origin]));
        }
        self.switch(func.span.end, after);
        self.live = live;
    }

    fn walk_function(&mut self, id: FuncId, func: &'a FuncBody) {
        let frame = self.frame();
        frame.created = frame.loops.first().map_or(func.span.start, |start| (*start).min(func.span.start));
        let mut entry = self.state.clone();
        for local in self.captured.get(&id).into_iter().flatten() {
            let decl = self.resolution.local(*local).func;
            let created = self.frames.iter().rev().find(|frame| frame.func == decl).map_or(0, |frame| frame.created);
            let later = self.writes[local].iter().filter(|write| write.func != decl || write.start >= created);
            let values = self.values(&entry, *local);
            entry.insert(*local, self.given(&values, later.map(|write| write.origin)));
        }
        self.frames.push(Frame::new(id));
        self.switch(func.params_span.end, entry);
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

    fn is_reassigned(&self, local: LocalId) -> bool {
        self.writes.contains_key(&local)
    }

    fn is_global(&self, name: &Name) -> bool {
        !matches!(self.resolution.resolve_at(name.span.start), Some(Resolved::Local(_)))
    }

    /// The local that a `type(value)` call checks, or with `true` a `math.type(value)` call.
    fn type_call(&self, expr: &Expr) -> Option<(LocalId, bool)> {
        let ExprKind::Call { callee, args, .. } = &expr.unparen().kind else { return None };
        let [arg] = args.as_slice() else { return None };
        let numbers = self.type_function(callee)?;
        Some((self.local(arg)?, numbers))
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
            ExprKind::Name(_) => *self.kinds.get(&self.local(side)?)?,
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
            // One side of a true `or` is true, and one side of a false `and` is false.
            ExprKind::Binary { op: BinOp::Or | BinOp::And, lhs, rhs, .. } => self.either(lhs, rhs, holds, out),
            ExprKind::Binary { op: op @ (BinOp::Eq | BinOp::Ne), lhs, rhs, .. } => {
                let equal = (*op == BinOp::Eq) == holds;
                let compared = match (self.local(lhs), literal(rhs)) {
                    (Some(local), Some(value)) => Some((local, value)),
                    _ => self.local(rhs).zip(literal(lhs)),
                };
                if let Some((local, value)) = compared {
                    out.push((local, if equal { Fact::Is(value) } else { Fact::IsNot(value) }));
                }
                if let Some((local, kind)) = self.kind_check(lhs, rhs).or_else(|| self.kind_check(rhs, lhs)) {
                    out.push((local, if equal { Fact::Kind(kind) } else { Fact::NotKind(kind) }));
                }
                // `data?.job == 'police'` and `data?.job ~= nil` read a value from `data`.
                for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                    if let (Some(local), Some(value)) = (self.read_from(side), literal(other)) {
                        if equal != (value == Type::Nil) {
                            out.push((local, Fact::Truthy));
                        }
                    }
                }
            }
            // `data?.job`, `data.job`, `data[key]` and `data:get()` are only true when `data` holds a
            // value: `?.` gives `nil` for a `nil` one, and the others raise an error.
            _ if holds => {
                if let Some(local) = self.read_from(cond) {
                    out.push((local, Fact::Truthy));
                }
            }
            _ => {}
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
                let before = self.state.clone();
                self.assume(lhs, *op == BinOp::And, rhs.span.start);
                self.visit_expr(rhs);
                let after = self.join(&before, &self.state);
                self.switch(rhs.span.end, after);
            }
            _ => visit::walk_expr(self, expr),
        }
    }
}

/// What a local may hold where two ways meet that leave it `a` and `b`: the values of both, each
/// with the facts that hold on both ways.
fn joined(a: &Values, b: &Values) -> Values {
    if a == b {
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
fn shared(a: &Rc<[(u32, Fact)]>, b: &Rc<[(u32, Fact)]>) -> Rc<[(u32, Fact)]> {
    if a == b {
        return a.clone();
    }
    a.iter().filter(|(_, fact)| b.iter().any(|(_, other)| other == fact)).cloned().collect()
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
    /// The code it holds in: from its line to the end of the block the statement after it is in,
    /// or to the end of the statement that next assigns the local. One on the last line of an `if`
    /// branch holds from the end of the `if`, and one on the last line of another block nowhere.
    pub span: Span,
}

/// The `---@cast` lines of a file.
#[derive(Debug, Default)]
pub struct Casts {
    casts: Vec<Cast>,
}

impl Casts {
    pub fn of(source: &str, chunk: &Chunk, resolution: &Resolution) -> Self {
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
            casts.push(Cast {
                local: resolution.lookup_local_at(cast.name, comment.span.start),
                entries: entries
                    .map(|(entry, at)| (entry, Span::new(base + at.start as u32, base + at.end as u32)))
                    .collect(),
                span: Span::empty(comment.span.end),
            });
        }
        if casts.is_empty() {
            return Self::default();
        }
        let mut locals: Vec<LocalId> = casts.iter().filter_map(|cast| cast.local).collect();
        locals.sort_unstable();
        locals.dedup();
        let writes = |local: LocalId| resolution.local(local).refs.iter().filter(|r| r.write).map(|r| r.span.start);
        let mut reach = Reach {
            tokens: &chunk.tokens,
            statements: Vec::new(),
            branch_ends: FxHashMap::default(),
            writes: locals.iter().flat_map(|local| writes(*local)).map(|at| (at, u32::MAX)).collect(),
        };
        reach.writes.sort_unstable();
        reach.visit_block(&chunk.block);
        reach.statements.sort_unstable();
        // For each local, where each write to it starts, with the earliest end of a statement that
        // assigns it from that write on.
        let mut assigned = FxHashMap::default();
        for local in locals {
            let mut ends: Vec<(u32, u32)> = writes(local).map(|at| (at, reach.end_of_write(at))).collect();
            ends.sort_unstable();
            let mut earliest = u32::MAX;
            for (_, end) in ends.iter_mut().rev() {
                earliest = earliest.min(*end);
                *end = earliest;
            }
            assigned.insert(local, ends);
        }
        for cast in &mut casts {
            let line_end = cast.span.start;
            let Some((start, end)) = reach.from(line_end) else { continue };
            let reassigned = cast.local.and_then(|local| {
                let ends: &Vec<(u32, u32)> = &assigned[&local];
                ends.get(ends.partition_point(|(at, _)| *at <= line_end)).map(|(_, end)| *end)
            });
            cast.span = Span::new(start, end.min(reassigned.unwrap_or(u32::MAX)).max(start));
        }
        Self { casts }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Cast> {
        self.casts.iter()
    }

    /// The casts of `local` that hold at `offset`, in the order of their lines.
    pub fn at(&self, local: LocalId, offset: u32) -> impl Iterator<Item = &Cast> {
        self.casts.iter().filter(move |cast| cast.local == Some(local) && cast.span.contains(offset))
    }
}

/// Finds where the casts of a file stop holding.
struct Reach<'a> {
    tokens: &'a [Token],
    /// Where each statement starts, with the end of the block it is in.
    statements: Vec<(u32, u32)>,
    /// Where each `elseif`, `else` and `end` of an `if` starts, with the end of that `if`.
    branch_ends: FxHashMap<u32, u32>,
    /// Where each write to the local of a cast starts, in order, with the end of the innermost
    /// statement it is in.
    writes: Vec<(u32, u32)>,
}

impl Reach<'_> {
    /// Where a cast whose line ends at `at` starts holding, and the end of the block it holds to:
    /// that of the statement after it. After the last statement of an `if` branch, a cast holds
    /// from the end of the `if`, as code types what the branch leaves that way, and after that of
    /// another block, such as a function or loop body, nowhere.
    fn from(&self, at: u32) -> Option<(u32, u32)> {
        let next = self.tokens.get(self.tokens.partition_point(|token| token.span.start < at))?;
        if let Some(end) = self.branch_ends.get(&next.span.start) {
            return self.from(*end);
        }
        if matches!(next.kind, TokenKind::End | TokenKind::Until | TokenKind::Eof) {
            return None;
        }
        let next = self.statements.partition_point(|(start, _)| *start < at);
        self.statements.get(next).map(|(_, end)| (at, *end))
    }

    /// The end of the innermost statement that the write starting at `at` is in.
    fn end_of_write(&self, at: u32) -> u32 {
        let index = self.writes.partition_point(|(start, _)| *start < at);
        self.writes.get(index).map_or(u32::MAX, |(_, end)| *end)
    }
}

impl<'ast> Visitor<'ast> for Reach<'_> {
    fn visit_block(&mut self, block: &'ast Block) {
        self.statements.extend(block.stmts.iter().map(|stmt| (stmt.span.start, block.span.end)));
        visit::walk_block(self, block);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // Statements are visited before those inside them, so the innermost one is seen last.
        let first = self.writes.partition_point(|(at, _)| *at < stmt.span.start);
        let after = self.writes.partition_point(|(at, _)| *at < stmt.span.end);
        for (_, end) in &mut self.writes[first..after] {
            *end = stmt.span.end;
        }
        if let StmtKind::If { branches, else_block } = &stmt.kind {
            let tokens = self.tokens;
            let before = |offset: u32| tokens[..tokens.partition_point(|token| token.span.start < offset)].last();
            let elseifs = branches.iter().skip(1).map(|branch| branch.keyword_span.start);
            let keyword = else_block.as_ref().and_then(|block| before(block.span.start));
            let keyword = keyword.filter(|token| token.kind == TokenKind::Else).map(|token| token.span.start);
            let end = before(stmt.span.end).filter(|token| token.kind == TokenKind::End);
            for start in elseifs.chain(keyword).chain(end.map(|token| token.span.start)) {
                self.branch_ends.insert(start, stmt.span.end);
            }
        }
        visit::walk_stmt(self, stmt);
    }
}
