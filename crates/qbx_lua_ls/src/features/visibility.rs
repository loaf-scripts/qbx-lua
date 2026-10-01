//! Member visibility: `---@field private name type`, and `---@private`, `---@protected` or
//! `---@package` above the function or assignment that sets a member of a class. A private member is
//! used through the table the `---@class` annotation declares, or inside a function defined on that
//! table; a protected one also through or inside its subclasses; a package one in the file that
//! declares it. `invisible` reports the uses from anywhere else, completion leaves such members out,
//! and `missing-fields` does not ask for them. Only the language server knows the classes, so
//! qbx-lint registers the rule and this module reports it.
//!
//! As in LuaLS, the nearest class that declares a name decides, and one restrictive declaration
//! there makes the member restrictive. Unlike LuaLS, functions nested in a method count as inside
//! the class, as do the functions written in the table its `---@class` annotation declares.

use std::cell::OnceCell;
use std::sync::Arc;

use qbx_lua_analysis::project::relative_slash_path;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::lexer::TokenKind;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{SmolStr, Span};
use rustc_hash::FxHashSet;

use super::class_tables::Classes;
use crate::index::{ClassDef, FileId, SymbolKind};
use crate::infer::{table_fields, Infer};
use crate::luacats::{applies_on, Visibility};
use crate::types::Type;

const MAX_DEPTH: u32 = 8;

/// What keeps a member from code outside its class or file.
pub struct Restriction {
    pub visibility: Visibility,
    /// The class that declares the member.
    pub class: SmolStr,
    /// The file of the declaration that restricts it.
    pub file: FileId,
}

/// Where the code of one file may use the members of classes.
pub struct Scope<'a, 'b> {
    infer: &'a Infer<'b>,
    chunk: &'a Chunk,
    restricted: Arc<FxHashSet<SmolStr>>,
    /// The functions defined on the table of a class, with that class.
    methods: OnceCell<Vec<(Span, SmolStr)>>,
}

impl<'a, 'b> Scope<'a, 'b> {
    pub fn new(infer: &'a Infer<'b>, chunk: &'a Chunk) -> Self {
        Self { infer, chunk, restricted: infer.index.restricted_names(), methods: OnceCell::new() }
    }

    /// Whether code at `offset` may use the member `name` of a value of type `owner`, which `base`
    /// is when the code names it.
    pub fn allows(&self, owner: &Type, base: Option<&Expr>, name: &str, offset: u32) -> bool {
        self.hidden(owner, || base.and_then(|base| self.class_table(base)), name, offset).is_none()
    }

    /// The restriction that keeps code at `offset` from the member `name` of a value of type
    /// `owner`. `through` gives the class whose table the code uses the member through, if any.
    fn hidden(
        &self,
        owner: &Type,
        through: impl FnOnce() -> Option<SmolStr>,
        name: &str,
        offset: u32,
    ) -> Option<Restriction> {
        if !self.restricted.contains(name) {
            return None;
        }
        let mut classes = Vec::new();
        self.classes_of(owner, self.infer.ctx.file, &mut classes, 0);
        let declaration = |class: &str, from: FileId| self.declaration(class, from, name, &mut FxHashSet::default(), 0);
        let restrictions: Vec<Restriction> =
            classes.iter().filter_map(|(class, from)| declaration(class, *from).flatten()).collect();
        if restrictions.is_empty() {
            return None;
        }
        let through = through();
        restrictions.into_iter().find(|restriction| !self.sees(restriction, through.as_deref(), offset))
    }

    /// The classes a value of type `ty` may be, through unions and aliases, as `from` sees the names,
    /// each with the file whose view decides its declarations.
    fn classes_of(&self, ty: &Type, from: FileId, out: &mut Vec<(SmolStr, FileId)>, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        match ty {
            Type::Union(types) => types.iter().for_each(|part| self.classes_of(part, from, out, depth + 1)),
            Type::Named(name, _) => {
                let classes = Classes::new(self.infer);
                if classes.class_defs(name, from).is_empty() {
                    let aliases = classes.alias_defs(name, from);
                    aliases.iter().for_each(|(file, alias)| self.classes_of(&alias.ty, *file, out, depth + 1));
                } else if !out.iter().any(|(known, _)| known == name) {
                    out.push((name.clone(), from));
                }
            }
            _ => {}
        }
    }

    /// The declarations of the class `name` that `from` sees, as the other checks of classes read
    /// them: a resource's `Player` is not ox_lib's.
    fn class_defs(&self, name: &str, from: FileId) -> Vec<(FileId, &'b ClassDef)> {
        Classes::new(self.infer).class_defs(name, from)
    }

    /// How the nearest of `class` and its parents that declares `name` restricts it: `None` when
    /// none declares it, `Some(None)` when it is public. Members set through values of the class
    /// only count when they say `---@private` or the like. `from` is the file whose view of the
    /// type names decides which declarations of `class` count. A class that several parents share
    /// is read once.
    fn declaration(
        &self,
        class: &str,
        from: FileId,
        name: &str,
        visited: &mut FxHashSet<SmolStr>,
        depth: u32,
    ) -> Option<Option<Restriction>> {
        if depth > MAX_DEPTH || !visited.insert(SmolStr::new(class)) {
            return None;
        }
        let index = self.infer.index;
        let defs = self.class_defs(class, from);
        let mut declared = false;
        let mut found: Option<Restriction> = None;
        let mut note = |visibility: Visibility, file: FileId| {
            declared = true;
            if visibility > found.as_ref().map_or(Visibility::Public, |restriction| restriction.visibility) {
                found = Some(Restriction { visibility, class: SmolStr::new(class), file });
            }
        };
        for (file, def) in &defs {
            let sides = def.field_sides.iter().copied().chain(std::iter::repeat(None));
            let visibility = def.field_visibility.iter().copied().chain(std::iter::repeat(Visibility::Public));
            for ((field, side), visibility) in def.fields.iter().zip(sides).zip(visibility) {
                if field.name == name && applies_on(side, self.infer.side()) {
                    note(visibility, *file);
                }
            }
        }
        for (file, member) in index.members_named(class, name, self.infer.ctx.file) {
            if !member.injected || member.visibility != Visibility::Public {
                note(member.visibility, file);
            }
        }
        if declared {
            return Some(found);
        }
        let mut parents = defs.iter().flat_map(|(file, def)| def.parents.iter().map(move |parent| (parent, *file)));
        parents.find_map(|(parent, file)| self.declaration(parent, file, name, visited, depth + 1))
    }

    /// Whether code at `offset`, using the member through the table of the class `through`, if any,
    /// may use a member restricted so.
    fn sees(&self, restriction: &Restriction, through: Option<&str>, offset: u32) -> bool {
        let inside = |class: &str| match restriction.visibility {
            Visibility::Public => true,
            Visibility::Package => false,
            Visibility::Private => class == restriction.class,
            Visibility::Protected => {
                self.inherits(class, self.infer.ctx.file, &restriction.class, &mut FxHashSet::default(), 0)
            }
        };
        match restriction.visibility {
            Visibility::Package => restriction.file == self.infer.ctx.file,
            _ => {
                through.is_some_and(inside)
                    || self.methods().iter().any(|(span, class)| span.contains(offset) && inside(class))
            }
        }
    }

    /// Whether `class`, as `from` sees it, is `ancestor` or one of its subclasses. A class that
    /// several parents share is read once.
    fn inherits(
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
        if depth >= MAX_DEPTH || !visited.insert(SmolStr::new(class)) {
            return false;
        }
        let defs = self.class_defs(class, from);
        let mut parents = defs.iter().flat_map(|(file, def)| def.parents.iter().map(move |parent| (parent, *file)));
        parents.any(|(parent, file)| self.inherits(parent, file, ancestor, visited, depth + 1))
    }

    /// The class whose table `expr` is: a name or field that a `---@class` annotation declares, like
    /// `Secret` below `---@class Secret` `local Secret = {}`, or ox_lib's `lib.array`.
    fn class_table(&self, expr: &Expr) -> Option<SmolStr> {
        let expr = expr.unparen();
        let declares = |base: &Expr, name: &str| {
            let member = self.infer.member(&self.infer.expr(base), name);
            member.is_some_and(|member| member.kind == SymbolKind::Table && matches!(member.ty, Type::Named(..)))
        };
        let is_table = match &expr.kind {
            ExprKind::Name(name) => self.infer.is_class_table(name),
            ExprKind::Field { base, name, .. } => declares(base, &name.text),
            ExprKind::Index { base, index, .. } => index.as_string().is_some_and(|key| declares(base, key)),
            _ => false,
        };
        match self.infer.expr(expr).without_nil() {
            Type::Named(class, _) if is_table => Some(class),
            _ => None,
        }
    }

    /// The type of the value `function a.b:c()` sets `c` on, and the class whose table that is.
    fn function_owner(&self, name: &FuncName) -> (Type, Option<SmolStr>) {
        let path = if name.method.is_some() { &name.path[..] } else { &name.path[..name.path.len().saturating_sub(1)] };
        let func_name = |path: &[Name]| FuncName {
            base: name.base.clone(),
            path: path.to_vec(),
            method: None,
            span: name.base.span,
        };
        let Some((last, parents)) = path.split_last() else {
            let owner = self.infer.func_name_owner_type(&func_name(&[]));
            let class = match owner.without_nil() {
                Type::Named(class, _) if self.infer.is_class_table(&name.base) => Some(class),
                _ => None,
            };
            return (owner, class);
        };
        let parent = self.infer.func_name_owner_type(&func_name(parents));
        let Some(member) = self.infer.member(&parent, &last.text) else { return (Type::Unknown, None) };
        let class = match &member.ty {
            Type::Named(class, _) if member.kind == SymbolKind::Table => Some(class.clone()),
            _ => None,
        };
        (member.ty, class)
    }

    fn methods(&self) -> &[(Span, SmolStr)] {
        self.methods.get_or_init(|| {
            let tokens = self.chunk.tokens.iter().rev();
            let last_token_end = tokens.filter(|token| token.kind != TokenKind::Eof).map(|token| token.span.end).next();
            let mut finder =
                Methods { scope: self, last_token_end: last_token_end.unwrap_or_default(), out: Vec::new() };
            finder.visit_block(&self.chunk.block);
            finder.out
        })
    }
}

impl Restriction {
    fn message(&self, name: &str, infer: &Infer) -> String {
        match self.visibility {
            Visibility::Private => {
                format!("Field `{name}` is private, it can only be accessed in class `{}`", self.class)
            }
            Visibility::Protected => format!(
                "Field `{name}` is protected, it can only be accessed in class `{}` and its subclasses",
                self.class
            ),
            Visibility::Package | Visibility::Public => {
                let entry = infer.index.file(self.file);
                let path = match (entry, entry.and_then(|entry| entry.resource).and_then(|r| infer.index.resource(r))) {
                    (Some(entry), Some(resource)) => relative_slash_path(&resource.root, &entry.path),
                    (Some(entry), None) => {
                        entry.path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
                    }
                    (None, _) => String::new(),
                };
                format!("Field `{name}` can only be accessed in same file `{path}`")
            }
        }
    }
}

/// Collects the functions defined on the table of a class: `function Secret:m()`, `function
/// Secret.m()`, `Secret.m = function() end`, and those the table's own constructor holds.
struct Methods<'s, 'a, 'b> {
    scope: &'s Scope<'a, 'b>,
    /// Where the last token of the file ends.
    last_token_end: u32,
    out: Vec<(Span, SmolStr)>,
}

impl Methods<'_, '_, '_> {
    /// The code `func` covers. A function that the file ends in without its `end`, as a method
    /// being written usually is, covers the rest of the file, where the cursor is.
    fn body(&self, func: &FuncBody) -> Span {
        if func.end_span.is_empty() && func.span.end >= self.last_token_end {
            Span::new(func.span.start, u32::MAX)
        } else {
            func.span
        }
    }

    /// The functions that the table constructors in `exprs` hold, when a `---@class` annotation
    /// above the statement at `stmt_start` declares them.
    fn class_constructors(&mut self, stmt_start: u32, exprs: &[Expr]) {
        let doc = self.scope.infer.ctx.doc_at(stmt_start);
        let Some(class) = doc.declared_class() else { return };
        for fields in exprs.iter().filter_map(table_fields) {
            for field in fields {
                let (TableField::Named { value, .. } | TableField::Keyed { value, .. }) = field else { continue };
                if let ExprKind::Function(func) = &value.unparen().kind {
                    self.out.push((self.body(func), class.name.clone()));
                }
            }
        }
    }
}

impl<'c> Visitor<'c> for Methods<'_, '_, '_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        match &stmt.kind {
            StmtKind::Function { name, func } if name.method.is_some() || !name.path.is_empty() => {
                if let (_, Some(class)) = self.scope.function_owner(name) {
                    self.out.push((self.body(func), class));
                }
            }
            StmtKind::Local { exprs, .. } => self.class_constructors(stmt.span.start, exprs),
            StmtKind::Assign { targets, exprs } => {
                self.class_constructors(stmt.span.start, exprs);
                for (target, value) in targets.iter().zip(exprs) {
                    let ExprKind::Function(func) = &value.unparen().kind else { continue };
                    let base = match &target.kind {
                        ExprKind::Field { base, .. } => base,
                        ExprKind::Index { base, index, .. } if index.as_string().is_some() => base,
                        _ => continue,
                    };
                    if let Some(class) = self.scope.class_table(base) {
                        self.out.push((self.body(func), class));
                    }
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }
}

/// Each use of a member of a class where its visibility does not reach, with the message naming
/// who may use it: reads and writes through `.`, `:` and `["name"]`, and `function value:name()`.
/// Keys of table constructors are not uses, as in LuaLS, and neither is setting a member on the
/// table of a class, which declares that class's own member: a subclass may define the `__index`
/// its parent keeps private, as ox_lib's `function lib.ped:__index()` does.
pub fn invisible_members(infer: &Infer, chunk: &Chunk) -> Vec<(Span, String)> {
    let scope = Scope::new(infer, chunk);
    if scope.restricted.is_empty() {
        return Vec::new();
    }
    let mut finder = Finder { scope: &scope, declarations: Vec::new(), out: Vec::new() };
    finder.visit_block(&chunk.block);
    finder.out
}

struct Finder<'s, 'a, 'b> {
    scope: &'s Scope<'a, 'b>,
    /// The targets of assignments that set a member on the table of a class.
    declarations: Vec<Span>,
    out: Vec<(Span, String)>,
}

impl Finder<'_, '_, '_> {
    fn check(&mut self, owner: &Type, through: impl FnOnce() -> Option<SmolStr>, name: &str, span: Span) {
        if let Some(restriction) = self.scope.hidden(owner, through, name, span.start) {
            self.out.push((span, restriction.message(name, self.scope.infer)));
        }
    }
}

impl<'c> Visitor<'c> for Finder<'_, '_, '_> {
    fn visit_stmt(&mut self, stmt: &'c Stmt) {
        let scope = self.scope;
        match &stmt.kind {
            StmtKind::Function { name, .. } => {
                let set = name.method.as_ref().or(name.path.last());
                if let Some(set) = set.filter(|set| scope.restricted.contains(&set.text)) {
                    if let (owner, None) = scope.function_owner(name) {
                        self.check(&owner, || None, &set.text, set.span);
                    }
                }
            }
            StmtKind::Assign { targets, .. } => {
                for target in targets {
                    let (base, name) = match &target.kind {
                        ExprKind::Field { base, name, .. } => (base, &name.text),
                        ExprKind::Index { base, index, .. } => match index.as_string() {
                            Some(key) => (base, key),
                            None => continue,
                        },
                        _ => continue,
                    };
                    if scope.restricted.contains(name) && scope.class_table(base).is_some() {
                        self.declarations.push(target.span);
                    }
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'c Expr) {
        if self.declarations.contains(&expr.span) {
            return visit::walk_expr(self, expr);
        }
        let access = match &expr.kind {
            ExprKind::Field { base, name, .. } if !name.is_missing() => Some((base, &name.text, name.span)),
            ExprKind::MethodCall { base, method, .. } => Some((base, &method.text, method.span)),
            ExprKind::Index { base, index, .. } => index.as_string().map(|key| (base, key, index.span)),
            _ => None,
        };
        let scope = self.scope;
        if let Some((base, name, span)) = access.filter(|(_, name, _)| scope.restricted.contains(*name)) {
            self.check(&scope.infer.expr(base), || scope.class_table(base), name, span);
        }
        visit::walk_expr(self, expr);
    }
}
