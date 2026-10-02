use std::fmt::Write as _;
use std::sync::Arc;

use lsp_types::{Position, Range};
use qbx_fivem_data::Side;
use qbx_lua_analysis::env::is_meta_file;
use qbx_lua_analysis::scope::{Resolution, Resolved};
use qbx_lua_analysis::side_guard::SideRegions;
use qbx_lua_analysis::summary::summarize;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{Comment, LineIndex, SmolStr, Span};

use crate::callback_wrappers::Wrapper;
use crate::index::{
    AliasDef, ClassDef, Element, EnumTable, EventDef, EventFamily, EventKind, FileId, FileIndex, Index, Member,
    Metatable, NuiCallbackDef, Read, Symbol, SymbolKind,
};
use crate::infer::{metatable_args, table_elements, table_fields, FileContext, Infer};
use crate::luacats::{own_type, parse_doc_lines, DocGroup};
use crate::types::{CallbackRole, DescribedValue, FunType, Type};

const MAX_TABLE_DEPTH: u32 = 4;
const MAX_TABLE_FIELDS: usize = 400;
const MAX_MEMBERS_PER_FILE: usize = 6000;

pub fn span_to_range(source: &str, lines: &LineIndex, span: Span) -> Range {
    let start = lines.line_col_utf16(source, span.start);
    let end = lines.line_col_utf16(source, span.end.max(span.start));
    Range::new(Position::new(start.line, start.col), Position::new(end.line, end.col))
}

pub fn render_doc(doc: &DocGroup) -> Option<Arc<str>> {
    let mut out = doc.description.clone();
    if let Some(reason) = &doc.deprecated {
        let _ = write!(out, "\n\n**Deprecated** {reason}");
    }
    let documented: Vec<_> = doc.params.iter().filter(|p| !p.description.is_empty()).collect();
    if !documented.is_empty() {
        out.push('\n');
        for param in documented {
            let _ = write!(out, "\n- `{}`: {}", param.name, param.description);
        }
    }
    for ret in doc.returns.iter().filter(|r| !r.description.is_empty()) {
        let name = ret.name.as_deref().unwrap_or("returns");
        let _ = write!(out, "\n\n*{name}*: {}", ret.description);
    }
    // The values that `---|` lines list under a parameter or returned value, as lua-language-server
    // shows them, when any of them is described.
    let params = doc.params.iter().map(|param| (param.name.to_string(), &param.values));
    let returns = doc.returns.iter().enumerate().map(|(i, ret)| {
        let name = ret.name.as_ref().map_or_else(|| format!("return #{}", i + 1), SmolStr::to_string);
        (name, &ret.values)
    });
    let listed: Vec<String> = params
        .chain(returns)
        .filter(|(_, values)| values.iter().any(|v| !v.description.is_empty()))
        .map(|(name, values)| format!("{name}:{}", described_values(values)))
        .collect();
    if !listed.is_empty() {
        let _ = write!(out, "\n\n```lua\n{}\n```", listed.join("\n\n"));
    }
    let out = out.trim();
    (!out.is_empty()).then(|| Arc::from(out))
}

/// Values listed by `---|` lines, one indented line each, with their descriptions as comments.
pub fn described_values(values: &[DescribedValue]) -> String {
    let mut out = String::new();
    for listed in values {
        let _ = write!(out, "\n    | {}", listed.value);
        if !listed.description.is_empty() {
            let _ = write!(out, " -- {}", listed.description);
        }
    }
    out
}

pub fn index_file(
    file: FileId,
    source: &str,
    chunk: &Chunk,
    resolution: &Resolution,
    index: &Index,
    side: Option<Side>,
) -> FileIndex {
    let (mut out, reads) = index.reads_while(|| {
        let ctx = FileContext::new(file, source, chunk, resolution);
        // The index may not hold this file yet, so its side is passed along.
        let infer = Infer::with_side(&ctx, index, side);
        let lines = LineIndex::new(source);
        let mut indexer = Indexer {
            file,
            source,
            lines: &lines,
            ctx: &ctx,
            infer: &infer,
            side,
            regions: SideRegions::of(source, chunk),
            nui_globals: crate::nui_callbacks::NuiGlobals::of(&ctx),
            out: FileIndex::default(),
            depth: 0,
        };
        indexer.doc_comments(&chunk.comments);
        indexer.block(&chunk.block);
        if let Some(stmt @ Stmt { kind: StmtKind::Return(exprs), .. }) = chunk.block.stmts.last() {
            indexer.module_return(stmt.span.start, exprs);
        }
        indexer.out
    });
    out.summary = summarize(source, chunk, resolution);
    out.meta = is_meta_file(source, chunk);
    // The globals the file reads anywhere, natives aside, count too: the code that uses them is
    // checked again when they change, even where its index entry does not hold their types.
    out.reads = reads;
    out.reads.extend(global_reads(resolution).into_iter().map(Read::Global));
    out.reads.sort_unstable();
    out.reads.dedup();
    out
}

/// The globals a file reads, natives aside.
fn global_reads(resolution: &Resolution) -> Vec<SmolStr> {
    let reads = resolution.globals.iter().filter(|global| !global.is_definition()).map(|global| global.name.clone());
    reads.filter(|name| qbx_fivem_data::native(name).is_none()).collect()
}

struct Indexer<'a> {
    file: FileId,
    source: &'a str,
    lines: &'a LineIndex,
    ctx: &'a FileContext<'a>,
    infer: &'a Infer<'a>,
    side: Option<Side>,
    regions: SideRegions,
    nui_globals: crate::nui_callbacks::NuiGlobals,
    out: FileIndex,
    depth: u32,
}

pub const CONVAR_CALLS: &[&str] = &[
    "GetConvar",
    "GetConvarInt",
    "GetConvarBool",
    "GetConvarFloat",
    "SetConvar",
    "SetConvarReplicated",
    "SetConvarServerInfo",
];

/// `Entity(x).state`, `LocalPlayer.state`, `Player(src).state` and `GlobalState`.
pub fn is_state_bag(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Field { name, .. } => name.text == "state",
        ExprKind::Name(name) => name.text == "GlobalState",
        _ => false,
    }
}

const NET_EVENT_CALLS: &[&str] = &["RegisterNetEvent", "RegisterServerEvent"];
const HANDLER_CALLS: &[&str] = &["AddEventHandler"];
const CALLBACK_CALLS: &[&str] = &["lib.callback.register"];
const TRIGGER_CALLS: &[&str] = &[
    "TriggerEvent",
    "TriggerServerEvent",
    "TriggerClientEvent",
    "TriggerLatentServerEvent",
    "TriggerLatentClientEvent",
    "lib.callback",
    "lib.callback.await",
];

impl<'a> Indexer<'a> {
    fn range(&self, span: Span) -> Range {
        span_to_range(self.source, self.lines, span)
    }

    fn is_global(&self, name: &Name) -> bool {
        matches!(self.ctx.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
    }

    /// Groups adjacent `---` comments and records the classes and aliases they declare. Like LuaLS,
    /// other comments between them are passed over; a blank line or code ends the group.
    fn doc_comments(&mut self, comments: &[Comment]) {
        let mut group: Vec<&Comment> = Vec::new();
        let mut previous_end: Option<u32> = None;
        for comment in comments {
            let adjacent = previous_end.is_some_and(|end| {
                let gap = &self.source[end as usize..comment.span.start as usize];
                gap.bytes().filter(|b| *b == b'\n').count() <= 1 && gap.trim().is_empty()
            });
            if !adjacent {
                self.flush_doc_group(&group);
                group.clear();
            }
            if comment.span.text(self.source).starts_with("---") {
                group.push(comment);
            }
            previous_end = Some(comment.span.end);
        }
        self.flush_doc_group(&group);
    }

    fn flush_doc_group(&mut self, group: &[&Comment]) {
        if !group.iter().any(|c| c.span.text(self.source).contains('@')) {
            return;
        }
        let lines: Vec<&str> =
            group.iter().map(|c| c.span.text(self.source).strip_prefix("---").unwrap_or_default()).collect();
        let doc = parse_doc_lines(&lines);
        for mut class in doc.classes {
            let range = self.range(group[class.line.min(group.len() - 1)].span);
            let field_sides = class.fields.iter().map(|field| field.side).collect();
            let field_visibility = class.fields.iter().map(|field| field.visibility).collect();
            let field_values = class.fields.iter_mut().map(|field| std::mem::take(&mut field.values)).collect();
            let fields = class
                .fields
                .into_iter()
                .map(|field| Symbol {
                    kind: if field.ty.as_fun().is_some() { SymbolKind::Method } else { SymbolKind::Field },
                    ty: if field.optional { field.ty.optional() } else { field.ty },
                    doc: (!field.description.is_empty()).then(|| Arc::from(field.description.as_str())),
                    deprecated: false,
                    literal: None,
                    range: self.range(group[field.line.min(group.len() - 1)].span),
                    name: field.name,
                })
                .collect();
            self.out.classes.push(ClassDef {
                name: class.name,
                generics: class.generics,
                parents: class.parents,
                parent_types: class.parent_types,
                fields,
                field_sides,
                field_visibility,
                field_values,
                indices: class.indices,
                literal_fields: class.literal_fields,
                call: class.call,
                operators: class.operators,
                doc: (!class.description.is_empty()).then(|| Arc::from(class.description.as_str())),
                range,
                side: class.side,
                strict: class.strict,
            });
        }
        for alias in doc.aliases {
            let range = self.range(group[alias.line.min(group.len() - 1)].span);
            self.out.aliases.push(AliasDef {
                name: alias.name,
                generics: alias.generics,
                ty: alias.ty,
                doc: (!alias.description.is_empty()).then(|| Arc::from(alias.description.as_str())),
                range,
                side: alias.side,
                values: alias.values,
                table: None,
            });
        }
    }

    fn owner_of(&self, ty: &Type) -> Option<SmolStr> {
        match ty {
            Type::Named(name, _) => Some(name.clone()),
            Type::GlobalTable(owner) => Some(owner.clone()),
            Type::Union(types) => {
                let owners: Vec<SmolStr> = types.iter().filter_map(|t| self.owner_of(t)).collect();
                owners.iter().find(|o| !o.starts_with('%')).or(owners.first()).cloned()
            }
            _ => None,
        }
    }

    /// The class a global was annotated with, preferring this file so the answer does not depend
    /// on whether the file is already part of the index.
    fn global_class(&self, name: &str) -> Option<SmolStr> {
        let own = self.out.globals.iter().rev().find(|s| s.name == name).map(|s| s.ty.clone());
        match own.unwrap_or_else(|| self.infer.global_type(name)) {
            Type::Named(class, _) => Some(class),
            _ => None,
        }
    }

    /// Whether `name` is the table a `---@class` annotation declares, preferring this file's globals
    /// like `global_class`.
    fn is_class_table(&self, name: &Name) -> bool {
        let own = self.out.globals.iter().rev().find(|s| s.name == name.text).filter(|_| self.is_global(name));
        own.map_or_else(|| self.infer.is_class_table(name), Symbol::is_class_table)
    }

    /// The type of `self` in `function Players:rename()` or `function Players.admin:promote()` when
    /// this file declares `Players`, which `Infer` only reads from the index and so misses until the
    /// file is part of it.
    fn own_self_type(&self, expr: &Expr) -> Option<Type> {
        let ExprKind::Name(name) = &expr.kind else { return None };
        let Some(Resolved::Local(id)) = self.ctx.resolution.resolve_at(name.span.start) else { return None };
        let Some(crate::infer::Decl::SelfParam { name: method }) =
            self.ctx.decl(self.ctx.resolution.local(id).decl.start)
        else {
            return None;
        };
        if !self.is_global(&method.base) {
            return None;
        }
        let global = self.out.globals.iter().rev().find(|s| s.name == method.base.text)?;
        Some(method.path.iter().fold(global.ty.clone(), |ty, segment| self.own_member_type(&ty, &segment.text)))
    }

    /// The type of the field `name` of `ty`, preferring what this file sets. A plain table whose
    /// fields this file sets stands for them, as `Infer` reads it once the file is indexed.
    fn own_member_type(&self, ty: &Type, name: &str) -> Type {
        let owner = self.owner_of(ty);
        let own = owner.as_ref().and_then(|owner| {
            self.out.members.iter().rev().find(|member| member.owner == *owner && member.symbol.name == name)
        });
        let member = match own {
            Some(member) => member.symbol.ty.clone(),
            None => self.infer.member(ty, name).map(|member| member.ty).unwrap_or_default(),
        };
        let nested = owner.map(|owner| SmolStr::new(format!("{owner}.{name}")));
        match nested {
            Some(nested)
                if matches!(member, Type::Table | Type::Unknown)
                    && self.out.members.iter().any(|member| member.owner == nested) =>
            {
                Type::GlobalTable(nested)
            }
            _ => member,
        }
    }

    /// Records a member set by the statement or table field whose doc comment starts at `doc_anchor`,
    /// which may declare it `---@private`.
    fn push_member(&mut self, owner: SmolStr, symbol: Symbol, injected: bool, doc_anchor: Option<u32>) {
        if self.out.members.len() < MAX_MEMBERS_PER_FILE {
            let visibility = doc_anchor.map(|anchor| self.ctx.doc_at(anchor).visibility).unwrap_or_default();
            self.out.members.push(Member { owner, symbol, injected, visibility });
        }
    }

    /// The symbol for `name = value`, registering nested table fields under `nested_owner`.
    /// `position` is where `name` is among the targets of its statement, which picks its `@type`.
    fn value_symbol(
        &mut self,
        name: &Name,
        value: Option<&Expr>,
        doc_anchor: u32,
        position: usize,
        nested_owner: &str,
        table_depth: u32,
    ) -> Symbol {
        let doc = self.ctx.doc_at(doc_anchor);
        let mut kind = SymbolKind::Variable;
        let ty = if let Some(class) = doc.declared_class() {
            if let Some(fields) = value.and_then(table_fields) {
                self.table_members(class.name.clone(), fields, table_depth + 1);
            }
            kind = SymbolKind::Table;
            own_type(&class.name, &class.generics)
        } else if let Some(ty) = doc.type_at(position) {
            ty.clone()
        } else {
            match value.map(|v| (&v.kind, v)) {
                // An empty table that `setmetatable` gives a metatable still has fields to look up.
                Some((_, expr))
                    if table_fields(expr).is_some_and(<[TableField]>::is_empty) && metatable_args(expr).is_empty() =>
                {
                    kind = SymbolKind::Table;
                    Type::Table
                }
                Some((_, expr)) if table_fields(expr).is_some() && table_depth < MAX_TABLE_DEPTH => {
                    let fields = table_fields(expr).unwrap_or_default();
                    kind = SymbolKind::Table;
                    if let Some(enum_name) = &doc.enum_name {
                        let (keys, side) = (doc.enum_keys, doc.enum_side);
                        self.enum_class(enum_name.clone(), keys, side, fields, name.span, nested_owner);
                    }
                    let owner = SmolStr::new(nested_owner);
                    self.table_members(owner.clone(), fields, table_depth + 1);
                    self.constructor_metatables(&owner, expr);
                    Type::GlobalTable(owner)
                }
                Some((ExprKind::Function(func), _)) => {
                    kind = SymbolKind::Function;
                    Type::Fun(Arc::new(self.infer.fun_type(func, Some(doc_anchor), false)))
                }
                Some((_, expr)) => self.infer.expr(expr).widen(),
                None => Type::Unknown,
            }
        };
        if matches!(ty, Type::GlobalTable(_)) {
            kind = SymbolKind::Table;
        }
        Symbol {
            name: name.text.clone(),
            kind,
            ty,
            doc: render_doc(&doc),
            deprecated: doc.deprecated.is_some(),
            literal: value.and_then(|v| self.literal_text(v)),
            range: self.range(name.span),
        }
    }

    /// Short literals are kept so hovers can show `Debug: boolean = true` without the source file.
    fn literal_text(&self, value: &Expr) -> Option<SmolStr> {
        let is_literal = match &value.kind {
            ExprKind::True | ExprKind::False | ExprKind::Number(_) | ExprKind::String(_) | ExprKind::JenkinsHash(_) => {
                true
            }
            ExprKind::Unary { op: UnOp::Neg, expr } => matches!(expr.kind, ExprKind::Number(_)),
            _ => false,
        };
        let text = value.span.text(self.source);
        (is_literal && text.len() <= 48 && !text.contains('\n')).then(|| SmolStr::new(text))
    }

    /// Declares the alias of an `---@enum` above a table constructor whose fields are indexed under
    /// `owner`: the union of its values, or with `keys` of its keys.
    fn enum_class(
        &mut self,
        name: SmolStr,
        keys: bool,
        side: Option<Side>,
        fields: &[TableField],
        span: Span,
        owner: &str,
    ) {
        let mut members = Vec::new();
        for field in fields {
            let (key, value) = match field {
                TableField::Named { name: key, value } => (Type::StringLit(key.text.clone()), value),
                TableField::Keyed { key, value } => (self.infer.expr(key), value),
                _ => continue,
            };
            let value = if keys { None } else { Some(self.infer.expr(value)) };
            members.push((key, value));
        }
        let values = members.iter().map(|(key, value)| value.clone().unwrap_or_else(|| key.clone()));
        let ty = Type::union(values);
        let table = (!keys).then(|| {
            let members = members.into_iter().filter_map(|(key, value)| Some((key, value?))).collect();
            Arc::new(EnumTable { owner: SmolStr::new(owner), members })
        });
        let range = self.range(span);
        self.out.aliases.push(AliasDef {
            name,
            generics: Vec::new(),
            ty,
            doc: None,
            range,
            side,
            values: Vec::new(),
            table,
        });
    }

    fn push_element(&mut self, owner: SmolStr, key: Option<Type>, value: Type) {
        if self.out.elements.len() < MAX_MEMBERS_PER_FILE {
            self.out.elements.push(Element { owner, key, value });
        }
    }

    fn push_metatable(&mut self, owner: SmolStr, metatable: &Expr) {
        if self.out.metatables.len() < MAX_MEMBERS_PER_FILE {
            let metatable = self.infer.expr(metatable);
            if !metatable.is_unknown() {
                self.out.metatables.push(Metatable { owner, metatable });
            }
        }
    }

    /// The metatables that `setmetatable({ ... }, metatable)` gives the table it builds, which is
    /// indexed as `owner`.
    fn constructor_metatables(&mut self, owner: &SmolStr, expr: &Expr) {
        for metatable in metatable_args(expr) {
            self.push_metatable(owner.clone(), metatable);
        }
    }

    /// `setmetatable(Child, { __index = Base })` on a table that has members of its own: a top-level
    /// local, a global or a field of one. A table written in the call takes the owner of the name it
    /// is assigned to instead, and a `---@class` keeps the fields it declares.
    fn metatable_call(&mut self, args: &[Expr]) {
        let [table, metatable, ..] = args else { return };
        if table_fields(table).is_some() {
            return;
        }
        if let Some(owner) = self.table_owner(table) {
            self.push_metatable(owner, metatable);
        }
    }

    /// The owner that the members of the table `expr` names are indexed under, unless a `---@class`
    /// declares it. A local names a table of its own only when it is declared with one at the top of
    /// the file; others hold tables, such as an instance that has the type of its class.
    fn table_owner(&self, expr: &Expr) -> Option<SmolStr> {
        if let Some(path) = expr.dotted_path().filter(|_| self.root_is_global(expr)) {
            if matches!(path.as_str(), "_ENV" | "_G") {
                return None;
            }
            let class = match &expr.kind {
                ExprKind::Name(root) => self.global_class(&root.text),
                _ => None,
            };
            let typed_as_class = class.is_some() || matches!(self.infer.expr(expr), Type::Named(..));
            return (!typed_as_class).then(|| SmolStr::new(path));
        }
        let ExprKind::Name(name) = &expr.kind else { return None };
        let Some(Resolved::Local(id)) = self.ctx.resolution.resolve_at(name.span.start) else { return None };
        let own = self.ctx.local_owner_key(self.ctx.resolution.local(id).decl.start);
        matches!(self.infer.expr(expr).without_nil(), Type::GlobalTable(owner) if owner == own).then_some(own)
    }

    fn table_members(&mut self, owner: SmolStr, fields: &[TableField], depth: u32) {
        let fields = &fields[..fields.len().min(MAX_TABLE_FIELDS)];
        for field in fields {
            let (name, value) = match field {
                TableField::Named { name, value } => (name.clone(), value),
                TableField::Keyed { key: Expr { kind: ExprKind::String(key), span }, value } => {
                    (Name { text: key.clone(), span: *span }, value)
                }
                TableField::Keyed { .. } | TableField::Positional(_) | TableField::SetMember(_) => continue,
            };
            let nested = format!("{owner}.{}", name.text);
            let mut symbol = self.value_symbol(&name, Some(value), name.span.start, 0, &nested, depth);
            if symbol.kind == SymbolKind::Variable {
                symbol.kind = SymbolKind::Field;
            }
            self.push_member(owner.clone(), symbol, false, Some(name.span.start));
        }
        let elements = table_elements(fields);
        if !elements.array.is_empty() {
            let array = Type::union(elements.array.iter().map(|value| self.infer.expr(value).widen()));
            self.push_element(owner.clone(), None, array);
        }
        if !elements.keyed.is_empty() {
            let (keys, values): (Vec<Type>, Vec<Type>) = elements
                .keyed
                .iter()
                .map(|(key, value)| (self.infer.expr(key).widen(), self.infer.expr(value).widen()))
                .unzip();
            self.push_element(owner, Some(Type::union(keys)), Type::union(values));
        }
    }

    fn block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    fn func_body(&mut self, func: &FuncBody) {
        self.depth += 1;
        self.block(&func.body);
        self.depth -= 1;
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Function { name, func } => {
                self.function_decl(stmt, name, func);
                self.func_body(func);
            }
            StmtKind::LocalFunction { func, .. } => self.func_body(func),
            StmtKind::Local { names, exprs, .. } => {
                if self.depth == 0 {
                    for (i, name) in names.iter().enumerate() {
                        if let Some(fields) = exprs.get(i).and_then(table_fields) {
                            let doc = self.ctx.doc_at(stmt.span.start);
                            let owner = match doc.declared_class() {
                                Some(class) => class.name.clone(),
                                None => {
                                    let owner = self.ctx.local_owner_key(name.name.span.start);
                                    if let Some(expr) = exprs.get(i) {
                                        self.constructor_metatables(&owner, expr);
                                    }
                                    owner
                                }
                            };
                            if let Some(enum_name) = &doc.enum_name {
                                let (keys, side) = (doc.enum_keys, doc.enum_side);
                                self.enum_class(enum_name.clone(), keys, side, fields, name.name.span, &owner);
                            }
                            self.table_members(owner, fields, 1);
                        }
                    }
                }
                exprs.iter().for_each(|e| self.expr(e));
            }
            StmtKind::Assign { targets, exprs } => {
                for (i, target) in targets.iter().enumerate() {
                    self.assignment(stmt, target, i, exprs.get(i));
                    self.expr(target);
                }
                exprs.iter().for_each(|e| self.expr(e));
            }
            StmtKind::CompoundAssign { expr, .. } => self.expr(expr),
            StmtKind::Expr(expr) => {
                self.call_stmt(stmt, expr);
                self.expr(expr);
            }
            StmtKind::Do(body) | StmtKind::Defer(body) => self.block(body),
            StmtKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            StmtKind::Repeat { body, cond } => {
                self.block(body);
                self.expr(cond);
            }
            StmtKind::If { branches, else_block } => {
                for branch in branches {
                    self.expr(&branch.cond);
                    self.block(&branch.block);
                }
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            StmtKind::NumericFor { body, .. } => self.block(body),
            StmtKind::GenericFor { exprs, body, .. } => {
                exprs.iter().for_each(|e| self.expr(e));
                self.block(body);
            }
            StmtKind::Return(exprs) => exprs.iter().for_each(|e| self.expr(e)),
            StmtKind::Break | StmtKind::Goto(_) | StmtKind::Label(_) | StmtKind::Error => {}
        }
    }

    fn function_decl(&mut self, stmt: &Stmt, name: &FuncName, func: &FuncBody) {
        let doc = self.ctx.doc_at(stmt.span.start);
        let is_method = name.method.is_some();
        let fun = Type::Fun(Arc::new(self.infer.fun_type(func, Some(stmt.span.start), is_method)));
        let last = name.method.as_ref().or(name.path.last()).unwrap_or(&name.base);
        let symbol = Symbol {
            name: last.text.clone(),
            kind: if is_method { SymbolKind::Method } else { SymbolKind::Function },
            ty: fun,
            doc: render_doc(&doc),
            deprecated: doc.deprecated.is_some(),
            literal: None,
            range: self.range(last.span),
        };
        if name.path.is_empty() && name.method.is_none() {
            if self.is_global(&name.base) {
                self.out.globals.push(symbol);
            }
            return;
        }
        let owner_path = if is_method { &name.path[..] } else { &name.path[..name.path.len() - 1] };
        let (mut owner, typed_as_class) = if self.is_global(&name.base) {
            match self.global_class(&name.base.text) {
                Some(class) => (class, true),
                None => (name.base.text.clone(), false),
            }
        } else {
            let base = FuncName { base: name.base.clone(), path: Vec::new(), method: None, span: name.base.span };
            let ty = self.infer.func_name_owner_type(&base);
            match self.owner_of(&ty) {
                Some(owner) => (owner, matches!(ty.without_nil(), Type::Named(..))),
                None => return,
            }
        };
        let mut injected = typed_as_class && owner_path.is_empty() && !self.is_class_table(&name.base);
        for segment in owner_path {
            // `function lib.array:map()` below `---@class Array` `lib.array = lib.class('Array')`
            // sets a method of the class on its table, as `function Array:map()` would.
            owner = match self.class_table_member(&owner, &segment.text) {
                Some(class) => {
                    injected = false;
                    class
                }
                None => SmolStr::new(format!("{owner}.{}", segment.text)),
            };
        }
        self.push_member(owner, symbol, injected, Some(stmt.span.start));
    }

    /// The class whose table the member `name` of `owner` is, when a `---@class` annotation declares
    /// it there, preferring this file like `global_class`. The exports of a resource stay under
    /// `exports.name`, where what definition files declare for them is looked up.
    fn class_table_member(&self, owner: &str, name: &str) -> Option<SmolStr> {
        if owner == "exports" {
            return None;
        }
        let own = self.out.members.iter().rev().find(|member| member.owner == owner && member.symbol.name == name);
        let symbol = match own {
            Some(member) => &member.symbol,
            None => {
                let members = self.infer.index.members_named(owner, name, self.file);
                members.into_iter().map(|(_, member)| &member.symbol).find(|symbol| symbol.is_class_table())?
            }
        };
        match &symbol.ty {
            Type::Named(class, _) if symbol.is_class_table() => Some(class.clone()),
            _ => None,
        }
    }

    fn assignment(&mut self, stmt: &Stmt, target: &Expr, position: usize, value: Option<&Expr>) {
        match &target.kind {
            ExprKind::Name(name) if self.is_global(name) => {
                let symbol = self.value_symbol(name, value, stmt.span.start, position, &name.text.clone(), 0);
                self.out.globals.push(symbol);
            }
            ExprKind::Field { base, name, .. } if !name.is_missing() => {
                self.member_assignment(stmt, base, name, position, value);
            }
            ExprKind::Index { base, index, .. } => {
                if let Some(text) = index.as_string() {
                    let name = Name { text: text.clone(), span: index.span };
                    self.member_assignment(stmt, base, &name, position, value);
                }
            }
            _ => {}
        }
    }

    fn member_assignment(&mut self, stmt: &Stmt, base: &Expr, name: &Name, position: usize, value: Option<&Expr>) {
        if let ExprKind::Name(root) = &base.kind {
            if self.is_global(root) && matches!(root.text.as_str(), "_ENV" | "_G") {
                let symbol = self.value_symbol(name, value, stmt.span.start, position, &name.text.clone(), 0);
                self.out.globals.push(symbol);
                return;
            }
        }
        let (owner, typed_as_class) = match base.dotted_path() {
            Some(path) if self.root_is_global(base) => {
                let own_class = match &base.kind {
                    ExprKind::Name(root) => self.global_class(&root.text),
                    _ => None,
                };
                match own_class.map(|class| Type::Named(class, Vec::new())).unwrap_or_else(|| self.infer.expr(base)) {
                    Type::Named(class, _) => (class, true),
                    _ => (SmolStr::new(path), false),
                }
            }
            _ => {
                let ty = self.own_self_type(base).unwrap_or_else(|| self.infer.expr(base));
                match self.owner_of(&ty) {
                    Some(owner) => (owner, matches!(ty.without_nil(), Type::Named(..))),
                    None => return,
                }
            }
        };
        let class_table = matches!(&base.kind, ExprKind::Name(root) if self.is_class_table(root));
        let nested = format!("{owner}.{}", name.text);
        let mut symbol = self.value_symbol(name, value, stmt.span.start, position, &nested, 1);
        if symbol.kind == SymbolKind::Variable {
            symbol.kind = SymbolKind::Field;
        }
        if owner == "exports" && self.out.members.len() < MAX_MEMBERS_PER_FILE {
            let doc = self.ctx.doc_at(stmt.span.start);
            if doc.ty.is_some() || !doc.classes.is_empty() {
                self.out.typed_exports.push(self.out.members.len() as u32);
            }
        }
        self.push_member(owner, symbol, typed_as_class && !class_table, Some(stmt.span.start));
    }

    fn root_is_global(&self, expr: &Expr) -> bool {
        let mut current = expr;
        loop {
            match &current.kind {
                ExprKind::Field { base, .. } | ExprKind::Index { base, .. } => current = base,
                ExprKind::Name(name) => return self.is_global(name),
                _ => return false,
            }
        }
    }

    fn module_return(&mut self, stmt_start: u32, exprs: &[Expr]) {
        let Some(first) = exprs.first() else { return };
        self.out.module_return = Some(match table_fields(first) {
            Some(fields) => {
                // `---@enum Name` above `return { ... }` declares the enum a module exports.
                let doc = self.ctx.doc_at(stmt_start);
                let owner = SmolStr::new(format!("%mod{}", self.file));
                if let Some(enum_name) = &doc.enum_name {
                    self.enum_class(enum_name.clone(), doc.enum_keys, doc.enum_side, fields, first.span, &owner);
                }
                self.table_members(owner.clone(), fields, 1);
                self.constructor_metatables(&owner, first);
                Type::GlobalTable(owner)
            }
            None => self.infer.expr(first).widen(),
        });
    }

    fn call_stmt(&mut self, stmt: &Stmt, expr: &Expr) {
        let ExprKind::Call { callee, args, .. } = &expr.kind else { return };
        if callee.dotted_path().as_deref() != Some("exports") {
            return;
        }
        let (Some(name_arg), Some(value)) = (args.first(), args.get(1)) else { return };
        let Some(name) = name_arg.as_string() else {
            self.out.dynamic_exports = true;
            return;
        };
        let doc = self.ctx.doc_at(stmt.span.start);
        let ty = match &value.kind {
            ExprKind::Function(func) => Type::Fun(Arc::new(self.infer.fun_type(func, Some(stmt.span.start), false))),
            ExprKind::Name(global) if self.is_global(global) => {
                let own = self.out.globals.iter().rev().find(|s| s.name == global.text).map(|s| s.ty.clone());
                own.unwrap_or_else(|| self.infer.expr(value))
            }
            _ => self.infer.expr(value),
        };
        let mut symbol_doc = render_doc(&doc);
        if symbol_doc.is_none() {
            symbol_doc = self.referenced_doc(value);
        }
        self.out.exports.push(Symbol {
            name: name.clone(),
            kind: SymbolKind::Export,
            ty,
            doc: symbol_doc,
            deprecated: doc.deprecated.is_some(),
            literal: None,
            range: self.range(name_arg.span),
        });
    }

    /// Docs of the function an export refers to by name, e.g. `exports('GetPlayer', GetPlayer)`.
    fn referenced_doc(&self, value: &Expr) -> Option<Arc<str>> {
        let ExprKind::Name(name) = &value.kind else { return None };
        match self.ctx.resolution.resolve_at(name.span.start)? {
            Resolved::Local(id) => {
                let decl = self.ctx.resolution.local(id).decl.start;
                let anchor = match self.ctx.decl(decl)? {
                    crate::infer::Decl::LocalFunction { stmt, .. } | crate::infer::Decl::Local { stmt, .. } => {
                        stmt.span.start
                    }
                    _ => return None,
                };
                render_doc(&self.ctx.doc_at(anchor))
            }
            Resolved::Global(_) => self.out.globals.iter().find(|s| s.name == name.text).and_then(|s| s.doc.clone()),
        }
    }

    fn state_key(&mut self, key: Option<&SmolStr>) {
        let Some(key) = key.filter(|k| !k.is_empty() && k.as_str() != "set") else { return };
        if !self.out.state_keys.contains(key) {
            self.out.state_keys.push(key.clone());
        }
    }

    /// ox_lib fills its cache through `cache:set('ped', ped)`, so the field names only exist as strings.
    fn keyed_setter(&mut self, base: &Expr, method: &Name, args: &[Expr]) {
        let is_cache = matches!(&base.kind, ExprKind::Name(name) if name.text == "cache");
        let (true, "set", [key, value, ..]) = (is_cache, method.text.as_str(), args) else { return };
        let (Some(field), Some(owner)) = (key.as_string(), self.owner_of(&self.infer.expr(base))) else { return };
        let symbol = Symbol {
            name: field.clone(),
            kind: SymbolKind::Field,
            ty: self.infer.expr(value).widen(),
            doc: None,
            deprecated: false,
            literal: None,
            range: self.range(key.span),
        };
        self.push_member(owner, symbol, false, None);
    }

    /// The handler a framework or `@callback` registration passes: a function literal typed by the
    /// doc comment above the statement of the call at `offset`, or a local function that is never
    /// reassigned.
    fn handler_type(&self, arg: &Expr, offset: u32) -> Option<Arc<FunType>> {
        match &arg.kind {
            ExprKind::Function(func) => {
                let anchor = self.ctx.call_doc_anchor(offset);
                Some(Arc::new(self.infer.fun_type(func, Some(anchor), false)))
            }
            ExprKind::Name(name) => {
                let Some(Resolved::Local(id)) = self.ctx.resolution.resolve_at(name.span.start) else {
                    return None;
                };
                if self.ctx.resolution.local(id).refs.iter().any(|reference| reference.write) {
                    return None;
                }
                self.infer.expr(arg).as_fun().cloned()
            }
            _ => None,
        }
    }

    /// A call to a `---@callback` wrapper: `RegisterServerCallback('name', handler)` registers a
    /// handler, while `AwaitServerCallback('name', ...)` and `TriggerCallback('name', cb, ...)` call one.
    fn wrapper_event(&mut self, base: &Expr, method: Option<&Name>, args: &[Expr], offset: u32) {
        // Without a literal name there is nothing to index, and most calls pass none.
        if !args.iter().any(|arg| arg.as_string().is_some()) {
            return;
        }
        let Some((fun, _)) = self.infer.callee_fun(base, method) else { return };
        let Some(wrapper) = Wrapper::of(&fun, method.is_some()) else { return };
        let Some(name_arg) = args.get(wrapper.arg(wrapper.name)) else { return };
        let Some(name) = name_arg.as_string().filter(|n| !n.is_empty()) else { return };
        let (kind, handler) = match wrapper.tag.role {
            CallbackRole::Register => {
                let handler = wrapper.function.and_then(|param| args.get(wrapper.arg(param)));
                (EventKind::Callback, handler.and_then(|arg| self.handler_type(arg, offset)))
            }
            CallbackRole::Await | CallbackRole::Trigger => (EventKind::Trigger, None),
        };
        self.out.events.push(EventDef {
            name: name.clone(),
            kind,
            family: wrapper.family(),
            side: self.regions.effective(offset, self.side),
            handler,
            range: self.range(name_arg.span),
        });
    }

    fn event(&mut self, callee: &Expr, args: &[Expr], offset: u32) {
        let framework = crate::framework_callbacks::classify(self.ctx, self.infer.index, callee);
        let side = self.regions.effective(offset, self.side);
        let (family, kind) = if let Some(call) = &framework {
            if side != Some(call.required_side()) {
                return;
            }
            (call.family.clone(), call.kind)
        } else {
            let Some(path) = callee.dotted_path() else { return self.wrapper_event(callee, None, args, offset) };
            let path = path.as_str();
            if NET_EVENT_CALLS.contains(&path) {
                (EventFamily::Native, EventKind::NetEvent)
            } else if HANDLER_CALLS.contains(&path) {
                (EventFamily::Native, EventKind::Handler)
            } else if CALLBACK_CALLS.contains(&path) {
                (EventFamily::OxLib, EventKind::Callback)
            } else if TRIGGER_CALLS.contains(&path) {
                (if path.starts_with("lib.") { EventFamily::OxLib } else { EventFamily::Native }, EventKind::Trigger)
            } else {
                return self.wrapper_event(callee, None, args, offset);
            }
        };
        let Some(name_arg) = args.first() else { return };
        let Some(name) = name_arg.as_string().filter(|n| !n.is_empty()) else { return };
        let handler = if framework.is_some() {
            args.get(1).filter(|_| kind == EventKind::Callback).and_then(|arg| self.handler_type(arg, offset))
        } else {
            args.iter().skip(1).find_map(|arg| match &arg.kind {
                ExprKind::Function(func) => Some(Arc::new(self.infer.fun_type(func, None, false))),
                _ => None,
            })
        };
        self.out.events.push(EventDef {
            name: name.clone(),
            kind,
            family,
            side,
            handler,
            range: self.range(name_arg.span),
        });
    }

    fn nui_callback(&mut self, callee: &Expr, args: &[Expr], offset: u32) {
        let side = self.regions.effective(offset, self.side);
        if !matches!(side, Some(Side::Client | Side::Shared)) || self.side == Some(Side::Server) {
            return;
        }
        let Some(registration) = self.nui_globals.registration(self.ctx, callee) else { return };
        let [argument, _, ..] = args else { return };
        let Some(name) = argument.as_string().filter(|name| !name.is_empty()) else { return };
        if self
            .ctx
            .chunk
            .errors
            .iter()
            .any(|error| error.span.start < argument.span.end && error.span.end > argument.span.start)
        {
            return;
        }
        self.out.nui_callbacks.push(NuiCallbackDef {
            name: name.clone(),
            registration: registration.into(),
            range: self.range(argument.span),
        });
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Function(func) => self.func_body(func),
            ExprKind::Call { callee, args, .. } => {
                self.event(callee, args, expr.span.start);
                self.nui_callback(callee, args, expr.span.start);
                if let Some(path) = callee.dotted_path() {
                    let first = args.first().and_then(|a| a.as_string());
                    if CONVAR_CALLS.contains(&path.as_str()) {
                        if let Some(name) = first.filter(|n| !n.is_empty() && !self.out.convars.contains(n)) {
                            self.out.convars.push(name.clone());
                        }
                    } else if path == "AddStateBagChangeHandler" {
                        self.state_key(first);
                    } else if path == "setmetatable" {
                        self.metatable_call(args);
                    }
                }
                self.expr(callee);
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::MethodCall { base, method, args, .. } => {
                self.wrapper_event(base, Some(method), args, expr.span.start);
                self.keyed_setter(base, method, args);
                if let (true, "set", Some(key)) = (is_state_bag(base), method.text.as_str(), args.first()) {
                    self.state_key(key.as_string());
                }
                self.expr(base);
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::Index { base, index, .. } => {
                if is_state_bag(base) {
                    self.state_key(index.as_string());
                }
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Field { base, name, .. } => {
                if is_state_bag(base) {
                    self.state_key(Some(&name.text));
                }
                self.expr(base)
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Unary { expr, .. } | ExprKind::Paren(expr) => self.expr(expr),
            ExprKind::Table(fields) => {
                for field in fields {
                    match field {
                        TableField::Positional(value) | TableField::Named { value, .. } => self.expr(value),
                        TableField::Keyed { key, value } => {
                            self.expr(key);
                            self.expr(value);
                        }
                        TableField::SetMember(_) => {}
                    }
                }
            }
            _ => {}
        }
    }
}
