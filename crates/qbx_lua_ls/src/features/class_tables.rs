//! Table constructors typed as a LuaCATS class, found where their type is known: `---@type` locals
//! and assignments, arguments to class-typed parameters, `return` in a function documented with
//! `@return`, and the tables such fields hold. With the fields that code sets on and reads from
//! values of a class, they drive `missing-fields`, `assign-type-mismatch`, `undeclared-field` and
//! the completion of field names. Table constructors typed as a table type, like a shape
//! `{ value: string }`, `string[]` or `table<string, integer>`, are found in the same places for
//! `assign-type-mismatch`. Only the language server knows the classes, so qbx-lint registers the
//! rules and this module reports them.
//!
//! Type names are global, and resources may declare the same one differently: ox_fuel's
//! `@class State` is not qbx_vehicles' `@enum State`. A name is looked up the way the file that
//! uses it sees it, and a name that file cannot tell apart is not checked.

use std::sync::Arc;

use qbx_lua_analysis::scope::Resolved;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{NumberValue, SmolStr, Span};
use rustc_hash::{FxHashMap, FxHashSet};

use super::comparisons::Declared;
use super::unknown_types::is_typed;
use crate::index::{AliasDef, ClassDef, FileId, ResourceId};
use crate::infer::{bound_parents, class_bindings, expanded_bindings, substitute, Infer};
use crate::luacats::applies_on;
use crate::types::{DescribedValue, Type};

const MAX_DEPTH: u32 = 8;

/// A table constructor and the class it has to be.
pub struct ClassTable<'c> {
    pub class: SmolStr,
    /// The type arguments the class is given, as the `string` of `List<string>`.
    pub args: Vec<Type>,
    pub table: &'c Expr,
    /// The file whose view of the type names decided the class.
    pub from: FileId,
}

/// A `@field` of a class, or of one of its parents.
pub struct ClassField {
    pub name: SmolStr,
    pub ty: Type,
    pub doc: Option<Arc<str>>,
    /// The values that `---|` lines list under the field, with their descriptions.
    pub values: Vec<DescribedValue>,
    /// The file that declares the field, whose view of the type names its type is read with.
    pub file: FileId,
}

/// A field a constructor sets by name, `name = v`, `['name'] = v` or `.name`, with its value.
pub fn named_field(field: &TableField) -> Option<(&str, Option<&Expr>)> {
    match field {
        TableField::Named { name, value } => Some((name.text.as_str(), Some(value))),
        TableField::Keyed { key, value } => key.as_string().map(|name| (name.as_str(), Some(value))),
        TableField::SetMember(name) => Some((name.text.as_str(), None)),
        TableField::Positional(_) => None,
    }
}

/// What a table is indexed with: a field name, or the type of any other key.
pub enum Key<'k> {
    Name(&'k str),
    Typed(Type),
}

impl Key<'_> {
    pub fn ty(&self) -> Type {
        match self {
            Key::Name(name) => Type::StringLit(SmolStr::new(name)),
            Key::Typed(ty) => ty.clone(),
        }
    }

    /// How a message names the one field this key is: `name`, or `[1]` for an integer or boolean
    /// literal. Keys of other types, such as `integer`, name no single field.
    pub fn field(&self) -> Option<String> {
        match self {
            Key::Name(name) => Some(name.to_string()),
            Key::Typed(ty) if ty.is_literal() => Some(format!("[{ty}]")),
            Key::Typed(_) => None,
        }
    }
}

/// The type of a key that is no string. An integer or boolean written out, like the `1` of
/// `value[1]`, stays a literal to find its `---@field [1] number`; a variable is widened, since it
/// may hold other keys.
fn key_type(infer: &Infer, expr: &Expr) -> Type {
    match &expr.unparen().kind {
        ExprKind::Number(NumberValue::Int(i)) => Type::IntLit(*i),
        ExprKind::True => Type::BooleanLit(true),
        ExprKind::False => Type::BooleanLit(false),
        _ => infer.expr(expr).widen(),
    }
}

/// Each entry of a table constructor: its key, where the entry is written, and its value. Array
/// entries are keyed by their position, and `.name` sets `name` to `true` without a value to check.
pub fn entries<'c>(infer: &Infer, fields: &'c [TableField]) -> Vec<(Key<'c>, Span, Option<&'c Expr>)> {
    let mut position = 0;
    fields
        .iter()
        .map(|field| match field {
            TableField::Named { name, value } => (Key::Name(name.text.as_str()), name.span, Some(value)),
            TableField::SetMember(name) => (Key::Name(name.text.as_str()), name.span, None),
            TableField::Keyed { key, value } => match key.as_string() {
                Some(name) => (Key::Name(name.as_str()), key.span, Some(value)),
                None => (Key::Typed(key_type(infer, key)), key.span, Some(value)),
            },
            TableField::Positional(value) => {
                position += 1;
                (Key::Typed(Type::IntLit(position)), value.span, Some(value))
            }
        })
        .collect()
}

fn is_table(expr: &Expr) -> bool {
    matches!(expr.unparen().kind, ExprKind::Table(_))
}

/// Bits for the kinds of Lua value a type allows.
mod kind {
    pub const NIL: u8 = 1;
    pub const BOOLEAN: u8 = 2;
    pub const NUMBER: u8 = 4;
    pub const STRING: u8 = 8;
    pub const TABLE: u8 = 16;
    pub const FUNCTION: u8 = 32;
    pub const OTHER: u8 = 64;
}

/// Looks up the classes and aliases that a file sees.
pub struct Classes<'a, 'b> {
    infer: &'a Infer<'b>,
}

impl<'a, 'b> Classes<'a, 'b> {
    pub fn new(infer: &'a Infer<'b>) -> Self {
        Self { infer }
    }

    /// The file being analyzed.
    pub fn file(&self) -> FileId {
        self.infer.ctx.file
    }

    /// The classes and aliases called `name` that code in `from` sees, leaving those of definition
    /// files outside any resource out when its own resource or imports declare the name too. When it
    /// sees none, those of the one resource that declares the name count; several resources make it
    /// ambiguous.
    #[allow(clippy::type_complexity)]
    pub(crate) fn declarations(
        &self,
        name: &str,
        from: FileId,
    ) -> (Vec<(FileId, &'b ClassDef)>, Vec<(FileId, &'b AliasDef)>) {
        let index = self.infer.index;
        let side = self.infer.side();
        let mut classes = index.class_defs(name);
        classes.retain(|(_, class)| applies_on(class.side, side));
        let mut aliases = index.alias_defs(name);
        aliases.retain(|(_, alias)| applies_on(alias.side, side));
        let mut visible_classes: Vec<_> =
            classes.iter().copied().filter(|(file, _)| index.is_visible(from, *file)).collect();
        let mut visible_aliases: Vec<_> =
            aliases.iter().copied().filter(|(file, _)| index.is_visible(from, *file)).collect();
        let own = |file: &FileId| !index.falls_back_on(from, *file);
        if visible_classes.iter().map(|(file, _)| file).chain(visible_aliases.iter().map(|(file, _)| file)).any(own) {
            visible_classes.retain(|(file, _)| own(file));
            visible_aliases.retain(|(file, _)| own(file));
        }
        if !visible_classes.is_empty() || !visible_aliases.is_empty() {
            return (visible_classes, visible_aliases);
        }
        let files = classes.iter().map(|(file, _)| *file).chain(aliases.iter().map(|(file, _)| *file));
        let resources: FxHashSet<Option<ResourceId>> =
            files.map(|file| index.file(file).and_then(|f| f.resource)).collect();
        if resources.len() == 1 {
            (classes, aliases)
        } else {
            (Vec::new(), Vec::new())
        }
    }

    /// The classes called `name` that code in `from` sees, as `declarations` finds them.
    pub(crate) fn class_defs(&self, name: &str, from: FileId) -> Vec<(FileId, &'b ClassDef)> {
        self.declarations(name, from).0
    }

    /// The aliases and enums called `name` that code in `from` sees, as `declarations` finds them.
    pub(crate) fn alias_defs(&self, name: &str, from: FileId) -> Vec<(FileId, &'b AliasDef)> {
        self.declarations(name, from).1
    }

    /// `ty`, or the type of the one alias it names as `from` sees it, or the file that wrote the
    /// name as `Infer::view_of` finds it, with the type arguments of a generic alias in place of its
    /// parameters.
    fn resolve(&self, ty: &Type, from: FileId, depth: u32) -> Type {
        let Type::Named(name, args) = ty else { return ty.clone() };
        let from = self.infer.view_of(name, from);
        if depth >= MAX_DEPTH || !self.class_defs(name, from).is_empty() {
            return ty.clone();
        }
        match self.alias_defs(name, from).as_slice() {
            [(file, alias)] => {
                let ty = substitute(&alias.ty, &expanded_bindings(&alias.generics, args));
                self.resolve(&ty, *file, depth + 1)
            }
            _ => ty.clone(),
        }
    }

    /// The class a value of type `ty` must be, with the type arguments it is given, looking through
    /// `?` and aliases, and the file whose view of the names decides its declarations: `from`, or
    /// the file that wrote the name, as `Infer::view_of` finds it.
    pub fn class_of(&self, ty: &Type, from: FileId) -> Option<(SmolStr, Vec<Type>, FileId)> {
        match self.resolve(&ty.without_nil(), from, 0).without_nil() {
            Type::Named(name, args) => {
                let view = self.infer.view_of(&name, from);
                (!self.class_defs(&name, view).is_empty()).then_some((name.text, args, view))
            }
            _ => None,
        }
    }

    /// The table type a value of type `ty` must be, as `from` sees the names, looking through `?`
    /// and aliases: a shape like `{ value: string }`, an array like `string[]`, or a
    /// `table<K, V>`.
    pub fn table_type_of(&self, ty: &Type, from: FileId) -> Option<Type> {
        match self.resolve(&ty.without_nil(), from, 0).without_nil() {
            ty @ (Type::Shape(_) | Type::Array(_) | Type::Map(..)) => Some(ty),
            _ => None,
        }
    }

    /// The type that `key` of a table of the table type `ty`, as `from` sees it, holds: the field
    /// of a shape that a name names, or else the value of its array part or of an index that takes
    /// the key.
    pub fn table_field_type(&self, ty: &Type, from: FileId, key: &Key) -> Option<(Type, FileId)> {
        if let (Type::Shape(shape), Key::Name(name)) = (ty, key) {
            if let Some(field) = shape.fields.iter().find(|field| field.name == *name) {
                let ty = if field.optional { field.ty.clone().optional() } else { field.ty.clone() };
                return Some((ty, from));
            }
        }
        self.table_index_for(ty, from, &key.ty())
    }

    /// The `@field`s of `class`, given the type arguments `args`, and of its parents, with the fields
    /// of a table type it names as a parent, leaving out those scoped to the other side. The first
    /// declaration of a name wins, so a class can narrow a field of its parent. A class that several
    /// parents share is read once, and so is one that names itself as a parent with other type
    /// arguments.
    pub fn fields(&self, class: &str, args: &[Type], from: FileId) -> Vec<ClassField> {
        let mut out = Vec::new();
        self.collect_fields(class, args, from, &mut out, &mut FxHashSet::default(), 0);
        out
    }

    fn collect_fields(
        &self,
        class: &str,
        args: &[Type],
        from: FileId,
        out: &mut Vec<ClassField>,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) {
        if depth > MAX_DEPTH || !visited.insert(SmolStr::new(class)) {
            return;
        }
        let defs = self.class_defs(class, from);
        let bindings = class_bindings(&defs, args);
        for (file, def) in &defs {
            let sides = def.field_sides.iter().copied().chain(std::iter::repeat(None));
            for (i, (field, side)) in def.fields.iter().zip(sides).enumerate() {
                if applies_on(side, self.infer.side()) && !out.iter().any(|seen| seen.name == field.name) {
                    let (name, ty, doc) = (field.name.clone(), substitute(&field.ty, &bindings), field.doc.clone());
                    let values = def.field_values.get(i).cloned().unwrap_or_default();
                    out.push(ClassField { name, ty, doc, values, file: *file });
                }
            }
        }
        for (file, parent) in bound_parents(&defs, args) {
            match parent {
                Type::Named(parent, args) => self.collect_fields(&parent, &args, file, out, visited, depth + 1),
                Type::Shape(shape) => {
                    for field in &shape.fields {
                        if !out.iter().any(|seen| seen.name == field.name) {
                            let ty = if field.optional { field.ty.clone().optional() } else { field.ty.clone() };
                            out.push(ClassField { name: field.name.clone(), ty, doc: None, values: Vec::new(), file });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether the class `class`, as `from` sees it, is `ancestor` or extends it through its parents.
    pub fn extends(&self, class: &str, from: FileId, ancestor: &str) -> bool {
        self.extends_at(class, from, ancestor, &mut FxHashSet::default(), 0)
    }

    fn extends_at(
        &self,
        class: &str,
        from: FileId,
        ancestor: &str,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) -> bool {
        if class == ancestor {
            return true;
        }
        if depth > MAX_DEPTH || !visited.insert(SmolStr::new(class)) {
            return false;
        }
        for (file, def) in self.class_defs(class, from) {
            for parent in &def.parent_types {
                let Some((parent, _, view)) = self.class_of(parent, file) else { continue };
                if self.extends_at(&parent, view, ancestor, visited, depth + 1) {
                    return true;
                }
            }
        }
        false
    }

    /// Whether `class` is strict as `from` sees it: a declaration says `(strict)` or `(exact)`, or
    /// none says `(loose)` and `by_default` holds for the file of each.
    pub fn is_strict(&self, class: &str, from: FileId, by_default: impl Fn(FileId) -> bool) -> bool {
        let defs = self.class_defs(class, from);
        defs.iter().any(|(_, def)| def.strict == Some(true))
            || (!defs.is_empty() && defs.iter().all(|(file, def)| def.strict.is_none() && by_default(*file)))
    }

    /// The fields of `class`, given the type arguments `args`, and of its parents that are keyed by
    /// an integer or boolean literal, like `---@field [1] number`, with their values and the files
    /// that declare them. The first declaration of a key wins, as with named fields, and a class
    /// that several parents share is read once.
    fn literal_fields(&self, class: &str, args: &[Type], from: FileId) -> Vec<(Type, Type, FileId)> {
        let mut out = Vec::new();
        self.collect_literal_fields(class, args, from, &mut out, &mut FxHashSet::default(), 0);
        out
    }

    fn collect_literal_fields(
        &self,
        class: &str,
        args: &[Type],
        from: FileId,
        out: &mut Vec<(Type, Type, FileId)>,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) {
        if depth > MAX_DEPTH || !visited.insert(SmolStr::new(class)) {
            return;
        }
        let defs = self.class_defs(class, from);
        let bindings = class_bindings(&defs, args);
        for (file, def) in &defs {
            for (key, value) in def.literal_fields(self.infer.side()) {
                if !out.iter().any(|(seen, ..)| seen == key) {
                    out.push((key.clone(), substitute(value, &bindings), *file));
                }
            }
        }
        for (file, def) in &defs {
            for parent in &def.parent_types {
                if let Type::Named(parent, args) = substitute(parent, &bindings) {
                    self.collect_literal_fields(&parent, &args, *file, out, visited, depth + 1);
                }
            }
        }
    }

    /// Whether `class` or a parent takes `key`. A name is taken by an `@field` for this side, by a
    /// field set on the table a `---@class` declares, like `function Test:greet()`, or by an index
    /// such as `[string]`; a literal like `1` by its `---@field [1] number`; another key by an index
    /// of its type, or by a literal-keyed field it may be, as `integer` may be `1`. A table type
    /// named as a parent, like `{ [string]: any }` or `table<string, any>`, takes what its fields
    /// and indices take. A parent that is neither a class nor such a table type, like a plain
    /// `table`, takes anything, and so does a key whose type is not known well enough to rule out a
    /// field name.
    pub fn declares(&self, class: &str, from: FileId, key: &Key) -> bool {
        let named = match key {
            Key::Name(name) => {
                self.fields(class, &[], from).iter().any(|field| field.name == *name)
                    || self.members_declare(class, from, name, &mut FxHashSet::default(), 0)
            }
            Key::Typed(ty) => self.kinds(ty, self.file(), 0).is_none_or(|kinds| kinds & kind::STRING != 0),
        };
        let literal_fields = || self.literal_fields(class, &[], from);
        let literal = match key {
            Key::Typed(ty) if ty.is_literal() => literal_fields().iter().any(|(key, ..)| key == ty),
            Key::Typed(ty) => literal_fields().iter().any(|(key, _, file)| self.takes(key, *file, ty)),
            Key::Name(_) => false,
        };
        named
            || literal
            || self.index_for(class, &[], from, &key.ty(), &mut FxHashSet::default(), 0).is_some()
            || self.has_open_parent(class, from, &mut FxHashSet::default(), 0)
    }

    /// Whether the table that the `---@class` annotation of `class` or a parent declares sets `name`.
    /// A class that several parents share is read once.
    fn members_declare(
        &self,
        class: &str,
        from: FileId,
        name: &str,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) -> bool {
        if depth > MAX_DEPTH || !visited.insert(SmolStr::new(class)) {
            return false;
        }
        if self.infer.index.declared_members_of(class, self.file()).iter().any(|member| member.name == name) {
            return true;
        }
        let defs = self.class_defs(class, from);
        let mut parents = defs.iter().flat_map(|(file, def)| def.parents.iter().map(move |parent| (parent, *file)));
        parents.any(|(parent, file)| self.members_declare(parent, file, name, visited, depth + 1))
    }

    /// Whether `class` or one of its ancestors names a parent that is neither a class nor a table
    /// type with fields or indices, such as a plain `table`. A class that several parents share is
    /// read once, so parents that name each other are no open parent.
    fn has_open_parent(&self, class: &str, from: FileId, visited: &mut FxHashSet<SmolStr>, depth: u32) -> bool {
        if !visited.insert(SmolStr::new(class)) {
            return false;
        }
        let defs = self.class_defs(class, from);
        if defs.is_empty() || depth > MAX_DEPTH {
            return true;
        }
        let parents = defs.iter().flat_map(|(file, def)| def.parent_types.iter().map(move |parent| (parent, *file)));
        for (parent, file) in parents {
            let open = match parent {
                Type::Named(parent, _) => self.has_open_parent(parent, file, visited, depth + 1),
                Type::Shape(shape) => shape.fields.is_empty() && shape.array.is_none() && shape.indices.is_empty(),
                Type::Map(..) | Type::Array(_) => false,
                _ => true,
            };
            if open {
                return true;
            }
        }
        false
    }

    /// The type that `key` of a `class` table holds, given the type arguments `args` of the class,
    /// with the file whose view of the type names it is read with: the `@field` of a name or of a
    /// literal like `[1]`, or else the value of an index that takes the key. A key such as `integer`
    /// that may be several fields has no one type.
    pub fn field_type(&self, class: &str, args: &[Type], from: FileId, key: &Key) -> Option<(Type, FileId)> {
        let field = match key {
            Key::Name(name) => {
                self.fields(class, args, from).into_iter().find(|field| field.name == *name).map(|f| (f.ty, f.file))
            }
            Key::Typed(ty) if ty.is_literal() => {
                let mut fields = self.literal_fields(class, args, from).into_iter();
                fields.find(|(key, ..)| key == ty).map(|(_, v, file)| (v, file))
            }
            Key::Typed(_) => None,
        };
        field.or_else(|| self.index_for(class, args, from, &key.ty(), &mut FxHashSet::default(), 0))
    }

    /// The value type of the first index of `class`, given the type arguments `args`, or of a parent
    /// that takes keys of type `key`, inferred in this file, with the file that declares it. A table
    /// type named as a parent, like `{ [number]: T }` or `table<string, integer>`, counts as one. A
    /// class that several parents share is read once.
    fn index_for(
        &self,
        class: &str,
        args: &[Type],
        from: FileId,
        key: &Type,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) -> Option<(Type, FileId)> {
        if depth > MAX_DEPTH || !visited.insert(SmolStr::new(class)) {
            return None;
        }
        let defs = self.class_defs(class, from);
        let bindings = class_bindings(&defs, args);
        let own = defs.iter().find_map(|(file, def)| {
            let mut indices = def.indices(self.infer.side()).into_iter();
            let (_, value) =
                indices.find(|(index_key, _)| self.takes(&substitute(index_key, &bindings), *file, key))?;
            Some((substitute(value, &bindings), *file))
        });
        if own.is_some() {
            return own;
        }
        bound_parents(&defs, args).into_iter().find_map(|(file, parent)| match parent {
            Type::Named(parent, args) => self.index_for(&parent, &args, file, key, visited, depth + 1),
            table => self.table_index_for(&table, file, key),
        })
    }

    /// The value type of the first entry of the table type `ty`, as `from` sees it, that takes keys
    /// of type `key`: its array part, a `[key]` entry, or the values of `table<K, V>` or `V[]`.
    fn table_index_for(&self, ty: &Type, from: FileId, key: &Type) -> Option<(Type, FileId)> {
        let entries: Vec<(&Type, &Type)> = match ty {
            Type::Shape(shape) => {
                let array = shape.array.iter().map(|value| (&Type::Integer, value));
                array.chain(shape.indices.iter().map(|(index, value)| (index, value))).collect()
            }
            Type::Map(index, value) => vec![(&**index, &**value)],
            Type::Array(value) => vec![(&Type::Integer, &**value)],
            _ => Vec::new(),
        };
        let (_, value) = entries.into_iter().find(|(index, _)| self.takes(index, from, key))?;
        Some((value.clone(), from))
    }

    /// Whether an index keyed by `index_key`, as `from` sees it, takes keys of type `key`: one of a
    /// kind it takes, and when both are literals, the same. An index of `'a'|'b'`, or of an alias of
    /// them, takes each of the two.
    fn takes(&self, index_key: &Type, from: FileId, key: &Type) -> bool {
        self.takes_at(index_key, from, key, 0)
    }

    fn takes_at(&self, index_key: &Type, from: FileId, key: &Type, depth: u32) -> bool {
        if depth > MAX_DEPTH {
            return true;
        }
        let (index_key, key) = (self.resolve(index_key, from, depth), self.resolve(key, self.file(), depth));
        if let Type::Union(parts) = &index_key {
            return parts.iter().any(|part| self.takes_at(part, from, &key, depth + 1));
        }
        if index_key.is_literal() && key.is_literal() {
            return index_key == key;
        }
        match (self.kinds(&index_key, from, 0), self.kinds(&key, self.file(), 0)) {
            (Some(wanted), Some(got)) => wanted & got != 0,
            _ => true,
        }
    }

    /// The type of `expr` for checking the value it stores or returns. A local that is assigned
    /// again after its declaration has the types of the values that may reach it there, and is
    /// `unknown` when one of them is.
    pub fn value_type(&self, expr: &Expr) -> Type {
        if let ExprKind::Name(name) = &expr.unparen().kind {
            let resolution = self.infer.ctx.resolution;
            if let Some(Resolved::Local(id)) = resolution.resolve_at(name.span.start) {
                let cast = self.infer.cast_after(expr.span.end);
                if cast.is_none() && resolution.local(id).refs.iter().any(|r| r.write) {
                    return self.infer.known_local_type_at(id, name.span.start);
                }
            }
        }
        self.infer.expr(expr)
    }

    /// The values that `exprs` give a `return` or an assignment, with where each is written. A call
    /// at the end gives every value it returns.
    pub fn values(&self, exprs: &[Expr]) -> Vec<(Type, Span)> {
        let mut values = Vec::new();
        for (i, expr) in exprs.iter().enumerate() {
            if i + 1 == exprs.len() && expr.is_call() {
                values.extend(self.infer.expr_multi(expr).into_iter().map(|ty| (ty, expr.span)));
            } else {
                values.push((self.value_type(expr), expr.span));
            }
        }
        values
    }

    /// Whether a field of type `ty` may be left out: `string?`, `string|nil`, `any` or no type.
    pub fn admits_nil(&self, ty: &Type, from: FileId) -> bool {
        self.admits_nil_at(ty, from, 0)
    }

    fn admits_nil_at(&self, ty: &Type, from: FileId, depth: u32) -> bool {
        if depth > MAX_DEPTH {
            return true;
        }
        match self.resolve(ty, from, depth) {
            Type::Nil | Type::Any | Type::Unknown => true,
            Type::Union(types) => types.iter().any(|t| self.admits_nil_at(t, from, depth + 1)),
            _ => false,
        }
    }

    /// Whether a value of type `given`, inferred in this file, can clearly not be stored where
    /// `expected` is declared in `from`.
    pub fn rejects(&self, expected: &Type, from: FileId, given: &Type) -> bool {
        match (self.kinds(expected, from, 0), self.kinds(given, self.file(), 0)) {
            (Some(wanted), Some(got)) if wanted & got == 0 => true,
            _ => self.literal_mismatch(expected, from, given),
        }
    }

    /// The member of `given`, inferred in this file, that can clearly not be stored where
    /// `expected` is declared in `from`, as the `number` of a `string|number` for a `string`: each
    /// member of a union has to fit, and one of no known type does.
    pub fn rejected_part(&self, expected: &Type, from: FileId, given: &Type) -> Option<Type> {
        let mut parts = Vec::new();
        self.flatten(given, self.file(), &mut parts, 0);
        parts.into_iter().find(|part| self.rejects(expected, from, part))
    }

    /// How a message shows `given`, of which `expected` rejects `part`: as it is written when `part`
    /// is a literal that `expected` does not list, like the `'c'` of `'a'|'c'`, and widened to its
    /// kinds otherwise.
    pub fn shown(&self, expected: &Type, from: FileId, given: &Type, part: &Type) -> Type {
        match self.literal_mismatch(expected, from, part) {
            true => given.clone(),
            false => given.widen(),
        }
    }

    /// The kinds of Lua value `ty` allows, as `kind` bits, or `None` when it may be anything.
    fn kinds(&self, ty: &Type, from: FileId, depth: u32) -> Option<u8> {
        if depth > MAX_DEPTH {
            return None;
        }
        Some(match ty {
            // Outside a parameter, `` `T` `` is the class that a string argument names, which may be
            // anything.
            Type::Unknown | Type::Any | Type::NameOf(_) => return None,
            Type::Nil => kind::NIL,
            Type::Boolean | Type::BooleanLit(_) => kind::BOOLEAN,
            Type::Number | Type::Integer | Type::IntLit(_) | Type::Handle(_) => kind::NUMBER,
            Type::String | Type::StringLit(_) => kind::STRING,
            Type::Table
            | Type::Array(_)
            | Type::Map(..)
            | Type::Tuple(_)
            | Type::Shape(_)
            | Type::GlobalTable(_)
            | Type::Exports(_)
            | Type::Require(_) => kind::TABLE,
            Type::Function | Type::Fun(_) => kind::FUNCTION,
            Type::Thread | Type::Userdata => kind::OTHER,
            Type::Variadic(inner) => return self.kinds(inner, from, depth + 1),
            Type::Named(name, args) => {
                let classes = self.class_defs(name, from);
                let aliases = self.alias_defs(name, from);
                match (classes.is_empty(), aliases.as_slice()) {
                    // Classes describe tables, and also userdata such as `vector3`.
                    (false, []) => kind::TABLE | kind::OTHER,
                    (true, [(file, alias)]) => {
                        let bindings = expanded_bindings(&alias.generics, args);
                        return match bindings.is_empty() {
                            true => self.kinds(&alias.ty, *file, depth + 1),
                            false => self.kinds(&substitute(&alias.ty, &bindings), *file, depth + 1),
                        };
                    }
                    // Unknown, a generic, or declared as more than one thing.
                    _ => return None,
                }
            }
            Type::Union(types) => {
                let mut kinds = 0;
                for part in types {
                    kinds |= self.kinds(part, from, depth + 1)?;
                }
                kinds
            }
        })
    }

    /// Whether `given` is a literal that `expected` lists only other literals for, like `'other'`
    /// for `'male'|'female'`.
    pub fn literal_mismatch(&self, expected: &Type, from: FileId, given: &Type) -> bool {
        if !matches!(given, Type::StringLit(_) | Type::IntLit(_) | Type::BooleanLit(_)) {
            return false;
        }
        let mut parts = Vec::new();
        self.flatten(expected, from, &mut parts, 0);
        let listed: Vec<&Type> = parts.iter().filter(|part| !matches!(part, Type::Nil)).collect();
        !listed.is_empty()
            && listed.iter().all(|part| matches!(part, Type::StringLit(_) | Type::IntLit(_) | Type::BooleanLit(_)))
            && !listed.contains(&given)
    }

    /// Whether no value is both a `left` and a `right`, with their names read as this file sees
    /// them: they are different kinds of value, or literals of which none is on both sides. A side
    /// that is only `nil` counts as shared, so that a check for a missing value is never ruled out.
    pub fn never_equal(&self, left: &Type, right: &Type) -> bool {
        let from = self.file();
        let (mut lefts, mut rights) = (Vec::new(), Vec::new());
        self.flatten(left, from, &mut lefts, 0);
        self.flatten(right, from, &mut rights, 0);
        let is_nil = |parts: &[Type]| parts.iter().all(|part| matches!(part, Type::Nil));
        if is_nil(&lefts) || is_nil(&rights) {
            return false;
        }
        let may_equal = |left: &Type, right: &Type| {
            if left.is_literal() && right.is_literal() {
                return left == right;
            }
            match (self.kinds(left, from, 0), self.kinds(right, from, 0)) {
                (Some(left), Some(right)) => left & right != 0,
                _ => true,
            }
        };
        !lefts.iter().any(|left| rights.iter().any(|right| may_equal(left, right)))
    }

    /// The parts of a union, with the aliases it names resolved.
    pub(crate) fn flatten(&self, ty: &Type, from: FileId, out: &mut Vec<Type>, depth: u32) {
        if depth > MAX_DEPTH {
            out.push(Type::Unknown);
            return;
        }
        match self.resolve(ty, from, depth) {
            Type::Union(types) => types.iter().for_each(|part| self.flatten(part, from, out, depth + 1)),
            other => out.push(other),
        }
    }
}

/// Every class-typed table constructor of `chunk`, outer tables before the ones they hold.
pub fn class_tables<'c>(infer: &Infer, chunk: &'c Chunk) -> Vec<ClassTable<'c>> {
    let mut finder = Finder {
        classes: Classes::new(infer),
        function_returns: Vec::new(),
        documented: FxHashMap::default(),
        out: Vec::new(),
        table_types: None,
    };
    finder.visit_block(&chunk.block);
    finder.out
}

/// A table constructor typed as a table type rather than a class: a shape like `{ value: string }`,
/// an array like `string[]`, or a `table<K, V>`, also through an alias such as `Box<string>`.
pub struct TypedTable<'c> {
    /// The table type, with the aliases that name it resolved.
    pub ty: Type,
    pub table: &'c Expr,
    /// The file whose view of the type names decided the type.
    pub from: FileId,
}

/// The table constructors of `chunk` typed as a class, as `class_tables` finds them, together with
/// those typed as a table type, and the class-typed ones that tables of such types hold.
fn typed_tables<'c>(infer: &Infer, chunk: &'c Chunk) -> (Vec<ClassTable<'c>>, Vec<TypedTable<'c>>) {
    let mut finder = Finder {
        classes: Classes::new(infer),
        function_returns: Vec::new(),
        documented: FxHashMap::default(),
        out: Vec::new(),
        table_types: Some((Declared::keeping_annotations(infer), Vec::new())),
    };
    finder.visit_block(&chunk.block);
    (finder.out, finder.table_types.map(|(_, typed)| typed).unwrap_or_default())
}

/// A field set or read outside a table constructor.
pub struct Access<'c> {
    /// The type of the value whose field it is.
    pub owner: Type,
    pub key: Key<'c>,
    /// Where the name or key is written.
    pub span: Span,
    /// What an assignment stores; `None` for reads and `function value:name()`.
    pub value: Option<&'c Expr>,
    /// Set on the table a `---@class` annotation declares, which may take new fields.
    pub on_class_table: bool,
}

/// The fields that assignments and `function value:name()` statements set and, with `reads`, the
/// fields and keys that expressions read, such as `value.name`, `value[1]` and `value:name()`.
pub fn accesses<'c>(infer: &Infer, chunk: &'c Chunk, reads: bool) -> Vec<Access<'c>> {
    let mut walker = Accesses { infer, reads, out: Vec::new() };
    walker.visit_block(&chunk.block);
    walker.out
}

struct Accesses<'a, 'b, 'c> {
    infer: &'a Infer<'b>,
    reads: bool,
    out: Vec<Access<'c>>,
}

impl<'c> Accesses<'_, '_, 'c> {
    /// The value an access goes through, and its key and where that is written.
    fn target(&self, expr: &'c Expr) -> Option<(&'c Expr, Key<'c>, Span)> {
        match &expr.kind {
            ExprKind::Field { base, name, .. } if !name.is_missing() => Some((base, Key::Name(&name.text), name.span)),
            ExprKind::Index { base, index, .. } => {
                let key = match index.as_string() {
                    Some(name) => Key::Name(name.as_str()),
                    None => Key::Typed(key_type(self.infer, index)),
                };
                Some((base, key, index.span))
            }
            _ => None,
        }
    }

    fn is_class_table(&self, expr: &Expr) -> bool {
        matches!(&expr.kind, ExprKind::Name(name) if self.infer.is_class_table(name))
    }
}

impl<'c> Visitor<'c> for Accesses<'_, '_, 'c> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        match &stmt.kind {
            StmtKind::Assign { targets, exprs } => {
                for (i, target) in targets.iter().enumerate() {
                    match self.target(target) {
                        Some((base, key, span)) => {
                            let on_class_table = self.is_class_table(base);
                            let owner = self.infer.expr(base);
                            let value = exprs.get(i);
                            self.out.push(Access { owner, key, span, value, on_class_table });
                            // The target's value and key are read.
                            visit::walk_expr(self, target);
                        }
                        None => self.visit_expr(target),
                    }
                }
                exprs.iter().for_each(|expr| self.visit_expr(expr));
                return;
            }
            StmtKind::Function { name, .. } => {
                // `function value:name()` sets `name` on `value`, and `function a.b.c()` sets `c` on `a.b`.
                let (path, field) = match (&name.method, name.path.split_last()) {
                    (Some(method), _) => (&name.path[..], Some(method)),
                    (None, Some((last, path))) => (path, Some(last)),
                    (None, None) => (&name.path[..], None),
                };
                if let Some(field) = field {
                    let on_class_table = path.is_empty() && self.infer.is_class_table(&name.base);
                    let owner =
                        FuncName { base: name.base.clone(), path: path.to_vec(), method: None, span: name.base.span };
                    let owner = self.infer.func_name_owner_type(&owner);
                    let key = Key::Name(&field.text);
                    self.out.push(Access { owner, key, span: field.span, value: None, on_class_table });
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'c Expr) {
        if self.reads {
            let read = match &expr.kind {
                ExprKind::MethodCall { base, method, .. } => Some((&**base, Key::Name(&method.text), method.span)),
                _ => self.target(expr),
            };
            if let Some((base, key, span)) = read {
                let owner = self.infer.expr(base);
                self.out.push(Access { owner, key, span, value: None, on_class_table: false });
            }
        }
        visit::walk_expr(self, expr);
    }
}

/// The class-typed table constructor directly around `offset`, for completing its field names. A
/// table nested in it that is not typed as a class is not its own.
pub fn class_table_at<'c>(infer: &Infer, chunk: &'c Chunk, offset: u32) -> Option<ClassTable<'c>> {
    let mut innermost = Innermost { offset, found: None };
    innermost.visit_block(&chunk.block);
    let span = innermost.found?;
    class_tables(infer, chunk).into_iter().find(|found| found.table.span == span)
}

/// The innermost table constructor whose braces hold `offset`.
struct Innermost {
    offset: u32,
    found: Option<Span>,
}

impl<'c> Visitor<'c> for Innermost {
    fn visit_expr(&mut self, expr: &'c Expr) {
        if !expr.span.contains_inclusive(self.offset) {
            return;
        }
        if matches!(expr.kind, ExprKind::Table(_)) && expr.span.start < self.offset && self.offset < expr.span.end {
            self.found = Some(expr.span);
        }
        visit::walk_expr(self, expr);
    }
}

/// Each class-typed table constructor that leaves out required fields, with the message naming them.
/// Fields that its class keeps from the code there, like `---@field private`, are required too, as
/// in LuaLS.
pub fn missing_fields(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    class_tables(infer, chunk)
        .into_iter()
        .filter_map(|found| {
            let ExprKind::Table(fields) = &found.table.kind else { return None };
            let given: Vec<&str> = fields.iter().filter_map(named_field).map(|(name, _)| name).collect();
            let missing: Vec<String> = classes
                .fields(&found.class, &found.args, found.from)
                .iter()
                .filter(|field| !classes.admits_nil(&field.ty, field.file) && !given.contains(&field.name.as_str()))
                .map(|field| format!("`{}`", field.name))
                .collect();
            let message = format!("Missing required fields in type `{}`: {}", found.class, missing.join(", "));
            (!missing.is_empty()).then_some((found.table.span, message))
        })
        .collect()
}

/// Each value that a typed table constructor or an assignment stores in a field that does not take
/// it, such as `test = 1` for `---@field test string` or `other = true` for `---@field [string]
/// number`, with the message naming both types. Tables typed as a shape, an array or a
/// `table<K, V>` are checked the same way, but only classes are checked in assignments. Only clear
/// cases count: a different kind of value, or a literal the field does not list.
pub fn mismatched_fields(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    checked_fields(infer, chunk, None)
}

/// Each value of no known type, of those `pick` picks, that a typed table constructor or an
/// assignment stores in a field whose type is declared, as `mismatched_fields` finds them.
pub fn unknown_fields(infer: &Infer, chunk: &Chunk, pick: &dyn Fn(&Expr) -> bool) -> Vec<(Span, String)> {
    checked_fields(infer, chunk, Some(pick))
}

fn checked_fields(infer: &Infer, chunk: &Chunk, unknowns: Option<&dyn Fn(&Expr) -> bool>) -> Vec<(Span, String)> {
    let classes = Classes::new(infer);
    let mut out = Vec::new();
    let mut check = |field: Option<(Type, FileId)>, key: &Key, value: &Expr| {
        let Some((ty, file)) = field else { return };
        let given = classes.value_type(value);
        let target = match key.field() {
            Some(field) => format!("field `{field}`"),
            None => format!("`[{}]`", key.ty()),
        };
        if let Some(pick) = unknowns {
            if given.is_unknown() && is_typed(&ty) && pick(value) {
                out.push((value.span, format!("The type of the value assigned to {target} of type `{ty}` is unknown")));
            }
            return;
        }
        if classes.rejects(&ty, file, &given) {
            let shown = if classes.literal_mismatch(&ty, file, &given) { given } else { given.widen() };
            out.push((value.span, format!("Cannot assign `{shown}` to {target} of type `{ty}`")));
        }
    };
    let (class_tables, typed_tables) = typed_tables(infer, chunk);
    for found in class_tables {
        let ExprKind::Table(fields) = &found.table.kind else { continue };
        for (key, _, value) in entries(infer, fields) {
            if let Some(value) = value {
                check(classes.field_type(&found.class, &found.args, found.from, &key), &key, value);
            }
        }
    }
    for found in typed_tables {
        let ExprKind::Table(fields) = &found.table.kind else { continue };
        for (key, _, value) in entries(infer, fields) {
            if let Some(value) = value {
                check(classes.table_field_type(&found.ty, found.from, &key), &key, value);
            }
        }
    }
    for access in accesses(infer, chunk, false) {
        // `value.field = nil` clears a field only when its type allows `nil`, as `string?` does.
        if let (Some(value), Some((class, args, view))) =
            (access.value, classes.class_of(&access.owner, classes.file()))
        {
            check(classes.field_type(&class, &args, view, &access.key), &access.key, value);
        }
    }
    out
}

struct Finder<'a, 'b, 'c> {
    classes: Classes<'a, 'b>,
    /// The `@return` types of the functions around the statement being visited, innermost last.
    function_returns: Vec<Vec<Type>>,
    /// The `@return` types of the functions whose doc comment was visited, by the start of their
    /// parameter list.
    documented: FxHashMap<u32, Vec<Type>>,
    out: Vec<ClassTable<'c>>,
    /// When tables typed as table types are looked for too, the reader of declared types that
    /// assignments without a `---@type` are typed by, and the tables found.
    table_types: Option<(Declared<'a, 'b>, Vec<TypedTable<'c>>)>,
}

impl<'c> Finder<'_, '_, 'c> {
    /// Records `expr` when it is a table constructor and `expected`, named as `from` sees it, is a
    /// class, or a table type when those are looked for, then the constructors it holds for fields
    /// of such types.
    fn table(&mut self, expected: &Type, expr: &'c Expr, from: FileId, depth: u32) {
        let table = expr.unparen();
        let ExprKind::Table(fields) = &table.kind else { return };
        if let Some((class, args, from)) = self.classes.class_of(expected, from) {
            self.out.push(ClassTable { class: class.clone(), args: args.clone(), table, from });
            if depth < MAX_DEPTH {
                for (key, _, value) in entries(self.classes.infer, fields) {
                    let Some(value) = value.filter(|value| is_table(value)) else { continue };
                    if let Some((ty, file)) = self.classes.field_type(&class, &args, from, &key) {
                        self.table(&ty, value, file, depth + 1);
                    }
                }
            }
            return;
        }
        if self.table_types.is_none() {
            return;
        }
        let Some(ty) = self.classes.table_type_of(expected, from) else { return };
        if depth < MAX_DEPTH {
            for (key, _, value) in entries(self.classes.infer, fields) {
                let Some(value) = value.filter(|value| is_table(value)) else { continue };
                if let Some((field, file)) = self.classes.table_field_type(&ty, from, &key) {
                    self.table(&field, value, file, depth + 1);
                }
            }
        }
        if let Some((_, typed)) = &mut self.table_types {
            typed.push(TypedTable { ty, table, from });
        }
    }

    /// A table this file's code builds, typed by a name this file sees.
    fn top_table(&mut self, expected: &Type, expr: &'c Expr) {
        self.table(expected, expr, self.classes.file(), 0);
    }

    /// The type that an assignment to `target` without a `---@type` gives a table: that of a
    /// class the target holds, or else the table type it is declared with, as its own `---@type`
    /// or `@param` declares it. What the target was assigned before only tells what it held then.
    fn assigned_type(&self, target: &Expr) -> Type {
        let ty = self.classes.infer.target_type(target);
        if self.classes.class_of(&ty, self.classes.file()).is_some() {
            return ty;
        }
        match &self.table_types {
            Some((declared, _)) => declared.target(target),
            None => ty,
        }
    }
}

impl<'c> Visitor<'c> for Finder<'_, '_, 'c> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        for (doc, functions) in self.classes.infer.ctx.function_docs(stmt) {
            for func in functions {
                let returns = doc.returns.iter().map(|r| self.classes.infer.doc_type_for(stmt, func, &r.ty)).collect();
                self.documented.insert(func.params_span.start, returns);
            }
        }
        match &stmt.kind {
            StmtKind::Local { exprs, .. } if exprs.iter().any(is_table) => {
                let doc = self.classes.infer.ctx.doc_at(stmt.span.start);
                // `---@class Name` above a table declares the class rather than an instance of it.
                if doc.declared_class().is_none() {
                    for (index, expr) in exprs.iter().enumerate() {
                        if let Some(ty) = doc.type_at(index) {
                            self.top_table(ty, expr);
                        }
                    }
                }
            }
            StmtKind::Assign { targets, exprs } if exprs.iter().any(is_table) => {
                let doc = self.classes.infer.ctx.doc_at(stmt.span.start);
                if doc.declared_class().is_none() {
                    let values = targets.iter().zip(exprs).enumerate();
                    for (index, (target, expr)) in values.filter(|(_, (_, expr))| is_table(expr)) {
                        let declared = doc.type_at(index).cloned();
                        let expected = declared.unwrap_or_else(|| self.assigned_type(target));
                        self.top_table(&expected, expr);
                    }
                }
            }
            StmtKind::Return(exprs) if exprs.iter().any(is_table) => {
                let expected = self.function_returns.last().cloned().unwrap_or_default();
                for (ty, expr) in expected.iter().zip(exprs) {
                    self.top_table(ty, expr);
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_func_body(&mut self, func: &'c FuncBody) {
        let returns = self.documented.remove(&func.params_span.start).unwrap_or_default();
        self.function_returns.push(returns);
        visit::walk_func_body(self, func);
        self.function_returns.pop();
    }

    fn visit_expr(&mut self, expr: &'c Expr) {
        let call = match &expr.kind {
            ExprKind::Call { callee, args, .. } => Some((&**callee, None, args.as_slice())),
            ExprKind::MethodCall { base, method, args, .. } => Some((&**base, Some(method), args.as_slice())),
            _ => None,
        };
        if let Some((base, method, args)) = call.filter(|(_, _, args)| args.iter().any(is_table)) {
            if let Some((fun, _)) = self.classes.infer.callee_fun(base, method) {
                let fun = self.classes.infer.call_signature(&fun, args, method.is_some(), base.span.start);
                let (skip_params, skip_args) = fun.call_offsets(method.is_some());
                for (param, arg) in fun.params.iter().skip(skip_params).zip(args.iter().skip(skip_args)) {
                    self.top_table(&param.ty, arg);
                }
            }
        }
        visit::walk_expr(self, expr);
    }
}
