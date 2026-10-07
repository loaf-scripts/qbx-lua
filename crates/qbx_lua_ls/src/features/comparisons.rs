//! `impossible-comparison`: an `==` or `~=` whose two sides can never be equal, so that it always
//! gives the same answer. The sides are different kinds of value, as a string and a number are, or
//! they list literals of which none is on both sides, as `"invalid"` is none of
//! `"active"|"busy"|"ready"`.
//!
//! A side only has a type that something declares: a literal, an operator, an annotation, a stub
//! or a native, also the callee of a call for the parameters of a function passed to it. What
//! inference reads from assigned values is left out, since `Config.Webhook = ''` tells what is
//! stored today and not that `false` never is. Comparisons with `nil` are left alone as well:
//! annotations often leave out the `?` of a value that may be missing, and the check for it is
//! deliberate.

use std::cell::{OnceCell, RefCell};

use qbx_fivem_data::native;
use qbx_lua_analysis::scope::{Local, LocalId, Resolution, Resolved};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{NumberValue, Span};
use rustc_hash::{FxHashMap, FxHashSet};

use super::class_tables::{Classes, Key};
use super::unknown_types::has_param_line;
use crate::infer::{assigned_value, distinct_bases, iterated_table, Bases, Decl, Expected, Infer, SetTypes};
use crate::narrow::{Origin, DECLARATION};
use crate::types::Type;

const MAX_DEPTH: u32 = 16;

/// Each `==` and `~=` of `chunk` whose sides share no value, with a message naming both types and
/// the answer the comparison always gives.
pub fn impossible_comparisons(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let mut finder = Finder { declared: Declared::new(infer), out: Vec::new() };
    finder.visit_block(&chunk.block);
    finder.out
}

struct Finder<'a, 'b> {
    declared: Declared<'a, 'b>,
    out: Vec<(Span, String)>,
}

/// Reads the types that expressions are declared to have.
pub struct Declared<'a, 'b> {
    infer: &'a Infer<'b>,
    classes: Classes<'a, 'b>,
    /// Whether a local that is assigned again keeps the type its annotation gives it.
    keeps_annotations: bool,
    /// Whether the `nil` or `false` that a function without `@return` returns on some path counts.
    returned_nils: bool,
    /// Whether only what annotations declare counts: a table constructor then gives the table it
    /// builds no type, rather than the `{ label: string }` of `{ label = 'x' }`, a generic function
    /// returns what its `@return` gives with the generics the arguments bind, and an entry such as
    /// `list[i]` or a loop variable has the type it is read as when an annotation declares the table
    /// it comes from.
    annotations_only: bool,
    /// Where the expression that each `--[[@as T]]` casts starts, by where it ends.
    cast_starts: OnceCell<FxHashMap<u32, u32>>,
    /// The table whose sequence each numeric `for` variable walks, by the variable.
    sequences: OnceCell<FxHashMap<LocalId, LocalId>>,
    /// The types the locals read so far are declared with.
    declarations: RefCell<FxHashMap<LocalId, Type>>,
    /// The declared types of the values that the declarations and assignments of locals that are
    /// assigned again give them, by the local and the origin of the value.
    origins: RefCell<FxHashMap<(LocalId, u32), Type>>,
    in_progress: RefCell<FxHashSet<(LocalId, u32)>>,
    /// The declared types of the values that each set of `Origin::Others` stands for, by the local
    /// and the origin of the set.
    others: RefCell<SetTypes>,
}

impl<'a, 'b> Declared<'a, 'b> {
    pub fn new(infer: &'a Infer<'b>) -> Self {
        Self {
            infer,
            classes: Classes::new(infer),
            keeps_annotations: false,
            returned_nils: false,
            annotations_only: false,
            cast_starts: OnceCell::new(),
            sequences: OnceCell::new(),
            declarations: RefCell::default(),
            origins: RefCell::default(),
            in_progress: RefCell::default(),
            others: RefCell::default(),
        }
    }

    /// Takes the `---@type` or `@param` of a local at its word, also where the local is assigned
    /// again. Completion reads the values to offer for it from there, which no diagnostic can
    /// rely on.
    pub fn keeping_annotations(infer: &'a Infer<'b>) -> Self {
        Self { keeps_annotations: true, ..Self::new(infer) }
    }

    /// Also takes a function without `@return` at its word where a `return` of it gives `nil` or
    /// `false`, as `return` and `return nil` do, as lua-language-server infers it. Running past the
    /// end of its body is no such `return`, unless `strict` asks for what TypeScript infers.
    pub fn with_returned_nils(infer: &'a Infer<'b>) -> Self {
        Self { returned_nils: true, ..Self::new(infer) }
    }

    /// Reads only what annotations declare: a table constructor gives the table it builds no type,
    /// as lua-language-server leaves the fields of such a table open, while a loop variable over a
    /// table an annotation declares has the type its loop gives it.
    pub fn annotations(infer: &'a Infer<'b>) -> Self {
        Self { annotations_only: true, ..Self::new(infer) }
    }

    /// The type of `expr` as far as it is declared, and `unknown` beyond that.
    pub fn of(&self, expr: &Expr) -> Type {
        self.declared(expr, 0)
    }

    fn declared(&self, expr: &Expr, depth: u32) -> Type {
        if depth > MAX_DEPTH {
            return Type::Unknown;
        }
        // `local node = self.tail --[[@as LruNode]]` declares what the field holds there.
        if let Some(cast) = self.cast(expr) {
            return cast;
        }
        let expr = expr.unparen();
        match &expr.kind {
            ExprKind::Name(name) => match self.infer.ctx.resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => self.local(id, name.span.start, depth),
                _ => self.global(&name.text),
            },
            ExprKind::Field { base, name, .. } => {
                self.narrowed_field(expr, self.field(base, &Key::Name(&name.text), true, depth))
            }
            ExprKind::Index { base, index, .. } => match index.as_string() {
                Some(name) => self.narrowed_field(expr, self.field(base, &Key::Name(name), true, depth)),
                None => {
                    let ty = self.field(base, &Key::Typed(self.key(index, depth)), true, depth);
                    match !self.infer.strict() && self.in_sequence(base, index) {
                        true => ty.without_nil(),
                        false => ty,
                    }
                }
            },
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                self.returned(expr).and_then(|values| values.into_iter().next()).unwrap_or_default()
            }
            // `-'5'` is a number, and a table with an `__unm` metamethod gives anything.
            ExprKind::Unary { op: UnOp::Neg, expr: operand } => match self.declared(operand, depth + 1).widen() {
                ty @ (Type::Number | Type::Integer) => ty,
                _ => Type::Unknown,
            },
            // `a or b` is either of them, and `a and b` may be what `a` holds.
            ExprKind::Binary { op: BinOp::And | BinOp::Or, .. } => Type::Unknown,
            // Vectors, and tables with metamethods, give other values than numbers.
            ExprKind::Binary {
                op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::IDiv | BinOp::Mod | BinOp::Pow,
                lhs,
                rhs,
                ..
            } => {
                let is_number = |operand: &Expr| {
                    let ty = self.declared(operand, depth + 1).widen();
                    matches!(ty, Type::Number | Type::Integer)
                };
                if is_number(lhs) && is_number(rhs) {
                    self.infer.expr(expr)
                } else {
                    Type::Unknown
                }
            }
            ExprKind::Table(_) if self.annotations_only => Type::Unknown,
            // Literals, and operators whose result is of one kind whatever their operands are.
            _ => self.infer.expr(expr),
        }
    }

    /// The type that a `--[[@as T]]` after `expr` casts it to. The comment casts the whole
    /// expression before it, so `s` keeps its own type in `'id:' .. s --[[@as string]]`.
    fn cast(&self, expr: &Expr) -> Option<Type> {
        let cast = self.infer.cast_after(expr.span.end)?;
        let starts = self.cast_starts.get_or_init(|| {
            let mut finder = CastTargets { infer: self.infer, starts: FxHashMap::default() };
            finder.visit_block(&self.infer.ctx.chunk.block);
            finder.starts
        });
        (starts.get(&expr.span.end) == Some(&expr.span.start)).then_some(cast)
    }

    /// The type of a local where it is read at `offset`: the declared types of the values that may
    /// reach the read, as the guards and casts around the read narrow them. A value whose type is not
    /// declared leaves the local `unknown`, unless a `---@cast` line types it. A `+T` entry adds to a
    /// type, so one that is not known stays so.
    fn local(&self, id: LocalId, offset: u32, depth: u32) -> Type {
        self.infer.narrowed_with(id, offset, &|origin| self.bases(id, origin, depth), true)
    }

    /// The declared type of the local `id` where it is read at `offset`, as `of` gives it for a read
    /// there.
    pub fn at(&self, id: LocalId, offset: u32) -> Type {
        self.local(id, offset, 0)
    }

    /// Whether the guards where the local `id` is read at `offset` rule out `value` for each of the
    /// values that may reach the read and be it.
    pub fn rules_out(&self, id: LocalId, offset: u32, value: &Type) -> bool {
        self.infer.rules_out_with(id, offset, value, &|origin| self.bases(id, origin, 0))
    }

    /// The declared types of the values that `origin` gives the local `id`: that of its value, or for
    /// a set of `Origin::Others` those of the values in it, each once. `None` while they are being
    /// found.
    ///
    /// Without `strict`, the values that code running at other times gives a local without
    /// `---@type` or `@param` hold no `nil`: lua-language-server reads such a local in another
    /// function as its declaration gives it, so `coords = nil` in one function leaves `coords.x` in
    /// another unchecked, as a `nil` assigned in the same function is not.
    fn bases(&self, id: LocalId, origin: u32, depth: u32) -> Option<Bases> {
        let flow = self.infer.ctx.flow();
        let Origin::Others(set) = flow.origin(origin) else { return self.origin(id, origin, depth).map(Bases::One) };
        if let Some(types) = self.others.borrow().get(&(id, origin)) {
            return Some(Bases::Set(types.clone()));
        }
        let drops_nil = !self.infer.strict() && self.infer.annotation(id).is_none();
        let given = flow.others(set).iter().map(|other| self.origin(id, *other, depth));
        let given = given.filter_map(|ty| match ty {
            Some(Type::Nil) if drops_nil => None,
            Some(ty) if drops_nil => Some(Some(ty.without_nil())),
            ty => Some(ty),
        });
        let (types, complete) = distinct_bases(given);
        if complete {
            self.others.borrow_mut().insert((id, origin), types.clone());
        }
        (!types.is_empty()).then_some(Bases::Set(types))
    }

    /// The declared type of the value that `origin` gives the local `id`, or `None` while it is
    /// being found. An assignment gives the declared type of the value, bounded by the annotation
    /// of the local, or the `---@type` above it.
    fn origin(&self, id: LocalId, origin: u32, depth: u32) -> Option<Type> {
        let local = self.infer.ctx.resolution.local(id);
        if origin == DECLARATION && !local.refs.iter().any(|r| r.write) {
            return Some(match depth {
                0 => self.declaration(id),
                _ => self.local_declaration(id, depth),
            });
        }
        let key = (id, origin);
        if let Some(ty) = self.origins.borrow().get(&key) {
            return Some(ty.clone());
        }
        // The values given before it first, and none given after it that are not known yet, as for
        // `Infer::origin_type`.
        if self.in_progress.borrow().iter().any(|(local, other)| *local == id && *other <= origin) {
            return None;
        }
        self.in_progress.borrow_mut().insert(key);
        for earlier in self.infer.ctx.flow().chained(id).iter().take_while(|earlier| **earlier < origin) {
            self.origin(id, *earlier, depth);
        }
        let ty = match self.infer.ctx.flow().origin(origin) {
            Origin::Declaration => self.declared_value(id, depth),
            Origin::Assignment { stmt, index } => self.assigned(id, origin, stmt, index, depth),
            Origin::Compound { stmt } => match &stmt.kind {
                StmtKind::CompoundAssign { op: BinOp::Concat, .. } => Type::String,
                _ => Type::Unknown,
            },
            Origin::Function { .. } => self.infer.origin_type(id, origin).unwrap_or_default(),
            Origin::Cast(cast) => self.cast_value(id, cast, depth),
            // Read through `bases`, value by value.
            Origin::Others(_) => Type::Unknown,
        };
        self.in_progress.borrow_mut().remove(&key);
        self.origins.borrow_mut().insert(key, ty.clone());
        Some(ty)
    }

    /// The type of the value that the `---@cast` line of number `cast` gives the local `id`: the
    /// type it names, or the declared type the local has there with the types it adds or takes out.
    /// Adding to a type that is not declared leaves one that is still not known.
    fn cast_value(&self, id: LocalId, cast: u32, depth: u32) -> Type {
        let Some(cast) = self.infer.ctx.casts().get(cast) else { return Type::Unknown };
        let held = match cast.replaces() {
            true => Type::Unknown,
            false => match self.local(id, cast.span.start, depth + 1) {
                Type::Unknown => return Type::Unknown,
                held => held,
            },
        };
        cast.entries.iter().fold(held, |ty, (entry, _)| self.infer.cast(ty, entry))
    }

    /// The declared type of value `index` of the assignment `stmt` to the local `id`, which gives it
    /// the value of `origin`.
    fn assigned(&self, id: LocalId, origin: u32, stmt: &Stmt, index: usize, depth: u32) -> Type {
        let StmtKind::Assign { exprs, .. } = &stmt.kind else { return Type::Unknown };
        let doc = self.infer.ctx.doc_at(stmt.span.start);
        if let Some(ty) = doc.type_at(index).filter(|_| doc.declared_class().is_none()) {
            return ty.clone();
        }
        let single = |expr: &Expr| self.declared(expr, depth + 1);
        // `name = tonumber(name) --[[@as number]]` casts the first value of the call.
        let multi = |call: &Expr| {
            let mut values = self.returned(call).unwrap_or_default();
            if let Some(cast) = self.cast(call) {
                match values.first_mut() {
                    Some(first) => *first = cast,
                    None => values.push(cast),
                }
            }
            values
        };
        let value = assigned_value(exprs, index, single, multi);
        match self.infer.annotation(id) {
            // A value with no declared type, as `name or 'none'`, still tells which parts of the
            // annotation it may be, but one of no known type at all, as `untyped()` gives, is
            // not declared to be any of them.
            Some(_) if value.is_unknown() => {
                let inferred =
                    assigned_value(exprs, index, |expr| self.infer.expr(expr), |expr| self.infer.expr_multi(expr));
                match inferred.is_unknown() {
                    true => Type::Unknown,
                    false => self.infer.origin_type(id, origin).unwrap_or_default(),
                }
            }
            Some(declared) => self.infer.bounded(&declared, &value),
            None => value,
        }
    }

    /// The type a local is declared with, without what the guards and casts after its declaration
    /// tell. One that is assigned again after its declaration is `unknown`, since its declared type
    /// may not be what it holds.
    pub fn declaration(&self, id: LocalId) -> Type {
        if let Some(ty) = self.declarations.borrow().get(&id) {
            return ty.clone();
        }
        let ty = self.local_declaration(id, 0);
        self.declarations.borrow_mut().insert(id, ty.clone());
        ty
    }

    fn local_declaration(&self, id: LocalId, depth: u32) -> Type {
        let local = self.infer.ctx.resolution.local(id);
        if local.refs.iter().any(|r| r.write) && !(self.keeps_annotations && self.is_annotated(local)) {
            return Type::Unknown;
        }
        self.declared_value(id, depth)
    }

    /// The type that the declaration of the local `id` gives it, whatever is assigned to it later.
    fn declared_value(&self, id: LocalId, depth: u32) -> Type {
        let local = self.infer.ctx.resolution.local(id);
        match self.infer.ctx.decl(local.decl.start) {
            Some(Decl::Local { stmt, index }) => self.local_value(id, stmt, *index, depth),
            Some(Decl::Param { .. }) if has_param_line(self.infer, local) => self.infer.local_type(id),
            Some(Decl::Param { expected: Some(Expected::Arg { .. }), .. }) => self.callback_param(id, depth),
            Some(Decl::LocalFunction { .. } | Decl::SelfParam { .. }) => self.infer.local_type(id),
            Some(Decl::NumericFor) => Type::Number,
            Some(Decl::GenericFor { stmt, .. }) if self.annotations_only => match iterated_table(stmt) {
                Some((table, _)) if !self.declared(table, depth + 1).is_unknown() => self.infer.local_type(id),
                _ => Type::Unknown,
            },
            // With `strict`, as TypeScript types the values of a loop over a typed table, a loop over
            // a table whose type declares its values gives them, which `pairs` and `ipairs` never
            // give as `nil`.
            Some(Decl::GenericFor { stmt, .. }) if self.infer.strict() => match iterated_table(stmt) {
                Some((table, _)) if declares_values(&self.infer.expand_aliases(&self.infer.expr(table), 0)) => {
                    self.infer.local_type(id).without_nil()
                }
                _ => Type::Unknown,
            },
            _ => Type::Unknown,
        }
    }

    /// What the callee of a call declares for the parameter `id` of a function passed to it, with the
    /// generics bound from what the other arguments of the call declare: `value` of
    /// `onValue('Player', function(value) end)` is a `Player` for `fun(value: T)` and a `` `T` ``.
    fn callback_param(&self, id: LocalId, depth: u32) -> Type {
        let declared = |arg: &Expr| self.declared(arg, depth + 1);
        self.infer.declared_callback_param(id, declared).unwrap_or_default()
    }

    /// Whether a `---@type` above its declaration or a `@param` line types the local.
    fn is_annotated(&self, local: &Local) -> bool {
        match self.infer.ctx.decl(local.decl.start) {
            Some(Decl::Local { stmt, index }) => self.infer.ctx.doc_at(stmt.span.start).type_at(*index).is_some(),
            Some(Decl::Param { .. }) => has_param_line(self.infer, local),
            _ => false,
        }
    }

    /// What the `local` statement `stmt` gives the name at `index`: its `---@type`, or the declared
    /// type of the value it takes. A literal written out is widened, as `local mode = 'dev'` is a
    /// setting to change.
    fn local_value(&self, id: LocalId, stmt: &Stmt, index: usize, depth: u32) -> Type {
        let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return Type::Unknown };
        let doc = self.infer.ctx.doc_at(stmt.span.start);
        if doc.type_at(index).is_some() || !doc.classes.is_empty() {
            return self.infer.local_type(id);
        }
        match exprs.get(index) {
            Some(value) => {
                let ty = self.declared(value, depth + 1);
                let is_literal = matches!(
                    value.unparen().kind,
                    ExprKind::True | ExprKind::False | ExprKind::Number(_) | ExprKind::String(_)
                );
                if is_literal {
                    ty.widen()
                } else {
                    ty
                }
            }
            // A value of the call that ends the list.
            None => {
                let returned = exprs.last().and_then(|last| self.returned(last));
                returned.and_then(|values| values.into_iter().nth(index + 1 - exprs.len())).unwrap_or_default()
            }
        }
    }

    /// The type of a global typed as one class by everything that sets it. Its other values are
    /// inferred from what is assigned to it. With `strict`, as TypeScript reads a declared global, a
    /// class that one of them declares wins, as it does for the type of the global, so
    /// `---@type Settings` above `Config = {}` types `Config` beside a `Config = Config or {}`.
    fn global(&self, name: &str) -> Type {
        let symbols = self.infer.index.globals_named(name, self.classes.file());
        if self.infer.strict() {
            let preferred = self.infer.preferred_global(&symbols).map(|(_, symbol)| &symbol.ty);
            if let Some(ty @ Type::Named(..)) = preferred {
                return ty.clone();
            }
        }
        match symbols.split_first() {
            Some(((_, first), rest))
                if matches!(first.ty, Type::Named(..)) && rest.iter().all(|(_, symbol)| symbol.ty == first.ty) =>
            {
                first.ty.clone()
            }
            _ => Type::Unknown,
        }
    }

    /// The type that an assignment to `target` has to store: what its local is declared with,
    /// whatever it holds before, or the declared type of the field or entry it sets, which no guard
    /// narrows, as the `{ name: string }` that `list[#list + 1]` of a `{ name: string }[]` takes.
    pub fn target(&self, target: &Expr) -> Type {
        match &target.kind {
            ExprKind::Name(name) => match self.infer.ctx.resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => self.declaration(id),
                _ => self.of(target),
            },
            ExprKind::Field { base, name, .. } => self.entry(base, &Key::Name(&name.text)),
            ExprKind::Index { base, index, .. } => match index.as_string() {
                Some(name) => self.entry(base, &Key::Name(name)),
                None => self.entry(base, &Key::Typed(self.key(index, 0))),
            },
            _ => self.of(target),
        }
    }

    /// The declared type of the field or entry `key` of `base` that an assignment sets: the
    /// `@field` or index of a class, or the entry of a table type an annotation declares. A table
    /// that a constructor builds declares nothing about what code adds to it, as
    /// `rows[#rows + 1] = {...}` after `local rows = { {...} }`.
    fn entry(&self, base: &Expr, key: &Key) -> Type {
        match self.field(base, key, false, 0) {
            ty if ty.is_unknown() && !self.annotations_only => {
                Declared::annotations(self.infer).field(base, key, true, 0)
            }
            ty if ty.is_unknown() => self.field(base, key, true, 0),
            ty => ty,
        }
    }

    /// `ty`, the declared type of the field that `expr` reads, without what the guards around the
    /// read rule out. A field with no declared type stays unknown.
    fn narrowed_field(&self, expr: &Expr, ty: Type) -> Type {
        match ty.is_unknown() {
            true => ty,
            false => self.infer.field_narrowed(expr, ty),
        }
    }

    /// The type that the `@field`s and indices of a class give `key` of `base`, or with `tables` also
    /// a table type it is declared as, like `{ name: string? }` or `table<string, T?>`. Fields of
    /// other tables are inferred from what is assigned to them.
    fn field(&self, base: &Expr, key: &Key, tables: bool, depth: u32) -> Type {
        let base = self.declared(base, depth + 1);
        let from = self.classes.file();
        let found = match self.classes.class_of(&base, from) {
            Some((class, args, from)) => self.classes.field_type(&class, &args, from, key),
            None if tables => {
                let table = self.classes.table_type_of(&base, from);
                table.and_then(|table| self.classes.table_field_type(&table, from, key))
            }
            None => None,
        };
        found.map(|(ty, _)| ty).unwrap_or_default()
    }

    /// The type of a key that is no string: an integer or boolean written out stays a literal, to
    /// find its `---@field [1] number`, and a key of no declared type may be any, which every index
    /// takes.
    fn key(&self, index: &Expr, depth: u32) -> Type {
        match &index.unparen().kind {
            ExprKind::Number(NumberValue::Int(i)) => Type::IntLit(*i),
            ExprKind::True => Type::BooleanLit(true),
            ExprKind::False => Type::BooleanLit(false),
            _ => self.declared(index, depth + 1).widen(),
        }
    }

    /// Whether `base[index]` reads an item of the sequence a numeric `for` walks, as `rows[i]` in
    /// `for i = 1, #rows do` or `for i = #rows, 1, -1 do`: the `#` of a sequence is a border of it,
    /// so the loop only reads values it holds, unless it assigns the table or the loop variable. With
    /// `strict`, such an item has the type the table declares for it, `nil` included, as TypeScript
    /// reads an indexed type.
    fn in_sequence(&self, base: &Expr, index: &Expr) -> bool {
        let resolution = self.infer.ctx.resolution;
        let local = |expr: &Expr| match &expr.unparen().kind {
            ExprKind::Name(name) => match resolution.resolve_at(name.span.start) {
                Some(Resolved::Local(id)) => Some(id),
                _ => None,
            },
            _ => None,
        };
        let (Some(table), Some(var)) = (local(base), local(index)) else { return false };
        let sequences = self.sequences.get_or_init(|| {
            let mut finder = Sequences { resolution, out: FxHashMap::default() };
            finder.visit_block(&self.infer.ctx.chunk.block);
            finder.out
        });
        sequences.get(&var) == Some(&table)
    }

    /// What `call` returns when its function declares it. A native documented as `boolean` can
    /// give scripts `1` instead of `true`, so code compares its result with either.
    fn returned(&self, call: &Expr) -> Option<Vec<Type>> {
        let declared = match self.annotations_only {
            true => self.infer.annotated_returns(call),
            false => self.infer.declared_returns(call),
        };
        let values = declared
            .or_else(|| self.callback_returns(call))
            .or_else(|| self.generic_returns(call))
            .or_else(|| self.returned_nils(call))?;
        if !self.is_native_call(call) {
            return Some(values);
        }
        let as_returned = |ty: Type| match ty {
            Type::Boolean => Type::union([Type::Boolean, Type::Integer]),
            other => other,
        };
        Some(values.into_iter().map(as_returned).collect())
    }

    /// What a call of a parameter returns when the callee of its function declares the function it
    /// holds: `get()` gives a `boolean` for `get` of `fun(get: fun(): T)` with `T` bound to one. The
    /// generics that the other arguments of that call declare bind values that are declared too.
    fn callback_returns(&self, call: &Expr) -> Option<Vec<Type>> {
        let ExprKind::Call { callee, args, .. } = &call.kind else { return None };
        let ExprKind::Name(name) = &callee.unparen().kind else { return None };
        let Some(Resolved::Local(id)) = self.infer.ctx.resolution.resolve_at(name.span.start) else { return None };
        if !matches!(self.infer.ctx.decl(self.infer.ctx.resolution.local(id).decl.start), Some(Decl::Param { .. })) {
            return None;
        }
        let fun = self.infer.fun_of(&self.declaration(id))?;
        let fun = self.infer.call_signature(&fun, args, false, call.span.start);
        fun.generics.is_empty().then(|| fun.returns.clone())
    }

    /// What a call of a generic returns when the declared types of its arguments bind its generics:
    /// `first(names)` gives a `string?` for `fun(list: V[]): V?` and a `names` declared as
    /// `string[]`. Bound from the values the call passes, as inference binds them, it would tell
    /// what the code passes today, not what it may.
    fn generic_returns(&self, call: &Expr) -> Option<Vec<Type>> {
        self.infer.declared_generic_returns(call, |arg| self.of(arg))
    }

    /// With `returned_nils`, what `call` returns where its function, without `@return`, returns
    /// `nil` or `false` on some path and something else on another: the values its `return`s pass
    /// there, and with `strict` the `nil` of running past the end of its body, as TypeScript infers
    /// `undefined` there. Where it never returns either, the value it returns is not declared.
    ///
    /// Without `strict`, a call through `exports` gives none: lua-language-server does not see the
    /// functions that resources export, as `exports['qb-core']:GetPlayer(source)` runs one whose
    /// `return nil` TypeScript would count.
    fn returned_nils(&self, call: &Expr) -> Option<Vec<Type>> {
        if !self.returned_nils || (!self.infer.strict() && self.through_exports(call)) {
            return None;
        }
        let missing = |part: &Type| matches!(part, Type::Nil | Type::BooleanLit(false));
        let values = self.infer.inferred_returns_of(call, self.infer.strict())?.into_iter().map(|ty| match &ty {
            Type::Union(parts) if parts.iter().any(missing) && !parts.iter().all(missing) => ty,
            _ => Type::Unknown,
        });
        Some(values.collect())
    }

    /// Whether `call` reaches its function through `exports`, as `exports.res:Fn()` does.
    fn through_exports(&self, call: &Expr) -> bool {
        let mut root = match &call.kind {
            ExprKind::Call { callee, .. } => &**callee,
            ExprKind::MethodCall { base, .. } => &**base,
            _ => return false,
        };
        while let ExprKind::Field { base, .. } | ExprKind::Index { base, .. } = &root.unparen().kind {
            root = base;
        }
        let ExprKind::Name(name) = &root.unparen().kind else { return false };
        name.text == "exports"
            && matches!(self.infer.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
    }

    fn is_native_call(&self, call: &Expr) -> bool {
        matches!(&call.kind, ExprKind::Call { callee, .. } if self.is_native(callee, 0))
    }

    /// Whether `callee` names a native, or a local that holds one as `local IsCamActive = IsCamActive`
    /// does.
    pub fn is_native(&self, callee: &Expr, depth: u32) -> bool {
        let ExprKind::Name(name) = &callee.unparen().kind else { return false };
        let ctx = self.infer.ctx;
        let Some(Resolved::Local(id)) = ctx.resolution.resolve_at(name.span.start) else {
            return self.infer.index.globals_named(&name.text, self.classes.file()).is_empty()
                && native(&name.text).is_some();
        };
        let Some(Decl::Local { stmt, index }) = ctx.decl(ctx.resolution.local(id).decl.start) else { return false };
        let StmtKind::Local { exprs, in_unpack: false, .. } = &stmt.kind else { return false };
        depth < MAX_DEPTH && exprs.get(*index).is_some_and(|value| self.is_native(value, depth + 1))
    }
}

impl<'c> Visitor<'c> for Finder<'_, '_> {
    fn visit_expr(&mut self, expr: &'c Expr) {
        if let ExprKind::Binary { op: op @ (BinOp::Eq | BinOp::Ne), lhs, rhs, .. } = &expr.kind {
            let (left, right) = (self.declared.of(lhs), self.declared.of(rhs));
            if self.declared.classes.never_equal(&left, &right) {
                let answer = if *op == BinOp::Eq { "false" } else { "true" };
                self.out.push((expr.span, format!("Comparing `{left}` with `{right}` is always {answer}")));
            }
        }
        visit::walk_expr(self, expr);
    }
}

/// Finds the expressions that `--[[@as T]]` comments cast: of those that end where one starts,
/// the outermost.
struct CastTargets<'a, 'b> {
    infer: &'a Infer<'b>,
    starts: FxHashMap<u32, u32>,
}

/// Finds the numeric `for` loops that walk the sequence of a local table from its start to its `#`,
/// or back, and assign neither the table nor their variable.
struct Sequences<'r> {
    resolution: &'r Resolution,
    out: FxHashMap<LocalId, LocalId>,
}

impl<'c> Visitor<'c> for Sequences<'_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        if let StmtKind::NumericFor { var, start, limit, step, body } = &stmt.kind {
            // A step written out, as `-1` is.
            let step = match step {
                Some(step) => int_literal(step),
                None => Some(1),
            };
            let length = match (int_literal(start), int_literal(limit), step) {
                (Some(first), None, Some(step)) if first >= 1 && step > 0 => Some(limit),
                (None, Some(last), Some(step)) if last >= 1 && step < 0 => Some(start),
                _ => None,
            };
            let table = length.and_then(|length| match &length.unparen().kind {
                ExprKind::Unary { op: UnOp::Len, expr } => match &expr.unparen().kind {
                    ExprKind::Name(name) => self.resolution.resolve_at(name.span.start),
                    _ => None,
                },
                _ => None,
            });
            let assigned = |id: LocalId| {
                let local = self.resolution.local(id);
                local.refs.iter().any(|r| r.write && body.span.contains(r.span.start))
            };
            if let (Some(Resolved::Local(table)), Some(Resolved::Local(var))) =
                (table, self.resolution.resolve_at(var.span.start))
            {
                if !assigned(table) && !assigned(var) {
                    self.out.insert(var, table);
                }
            }
        }
        visit::walk_stmt(self, stmt);
    }
}

/// The integer `expr` writes out, as `1` or `-1`.
fn int_literal(expr: &Expr) -> Option<i64> {
    match &expr.unparen().kind {
        ExprKind::Number(NumberValue::Int(i)) => Some(*i),
        ExprKind::Unary { op: UnOp::Neg, expr } => match expr.unparen().kind {
            ExprKind::Number(NumberValue::Int(i)) => Some(-i),
            _ => None,
        },
        _ => None,
    }
}

impl<'c> Visitor<'c> for CastTargets<'_, '_> {
    fn visit_expr(&mut self, expr: &'c Expr) {
        if self.infer.cast_after(expr.span.end).is_some() {
            let start = self.starts.entry(expr.span.end).or_insert(expr.span.start);
            *start = (*start).min(expr.span.start);
        }
        visit::walk_expr(self, expr);
    }
}

/// Whether a table of type `ty` declares the values it holds, as a class, an array, a map or a tuple
/// that annotations write do, rather than one that a constructor builds and code fills.
fn declares_values(ty: &Type) -> bool {
    matches!(ty, Type::Named(..) | Type::Array(_) | Type::Map(..) | Type::Tuple(_))
}
