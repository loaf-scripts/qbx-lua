//! Doc comment checks that need no other file: `@param` names that no parameter of the documented
//! function has, or that one doc comment repeats, aliases, enums and class fields that one file
//! declares twice for one side, fields that follow no class, operators and cast variables that do
//! not exist, and functions whose parameters and returned values the doc comments leave out.

use std::cell::OnceCell;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use qbx_fivem_data::Side;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{Comment, LineIndex, SmolStr, Span};
use qbx_luacats::luacats::{applies_on, declared_name, has_attribute, parse_cast, parse_doc_lines, DocGroup};
use qbx_luacats::types::{FunType, Type};
use rustc_hash::{FxHashMap, FxHashSet};

use super::arity::Callees;
use super::{FileInput, Sink};
use crate::env::{doc_blocks, is_meta_file};
use crate::rules;
use crate::scope::{LocalId, Resolution, Resolved};

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    let source = input.source;
    let wanted = |code, tags: &[&str]| sink.enabled(code).is_some() && tags.iter().any(|tag| source.contains(tag));
    // Definition files document stubs, which need not spell out the parameters they describe.
    let params = wanted(rules::UNDEFINED_DOC_PARAM, &["@param"]) && !is_meta_file(source, input.chunk);
    let aliases = wanted(rules::DUPLICATE_DOC_ALIAS, &["@alias", "@enum"]);
    let fields = wanted(rules::DUPLICATE_DOC_FIELD, &["@field"]);
    let globals = sink.enabled(rules::MISSING_GLOBAL_DOC).is_some();
    let exported = sink.enabled(rules::MISSING_LOCAL_EXPORT_DOC).is_some();
    let incomplete = wanted(rules::INCOMPLETE_SIGNATURE_DOC, &["@param", "@return"]);
    let repeated_params = wanted(rules::DUPLICATE_DOC_PARAM, &["@param"]);
    let classless_fields = wanted(rules::DOC_FIELD_NO_CLASS, &["@field"]);
    let operators = wanted(rules::UNKNOWN_OPERATOR, &["@operator"]);
    let casts = wanted(rules::UNKNOWN_CAST_VARIABLE, &["@cast"]);
    let tags = repeated_params || classless_fields || operators || casts;
    if !(params || aliases || fields || globals || exported || incomplete || tags) {
        return;
    }
    let blocks: Vec<DocBlock> = doc_blocks(source, &input.chunk.comments)
        .into_iter()
        .filter(|comments| comments.iter().any(|comment| comment.span.text(source).contains('@')))
        .map(|comments| DocBlock::new(source, comments))
        .collect();
    let lines = LineIndex::new(source);
    let code = OnceCell::new();
    let code = || code.get_or_init(|| Code::of(input.chunk, input.resolution, &lines));
    if params {
        undefined_params(input, &blocks, code, &lines, sink);
    }
    if globals || exported || incomplete {
        missing_docs(input, &blocks, code(), &lines, sink);
    }
    if aliases {
        duplicate_aliases(&blocks, &lines, sink);
    }
    if fields {
        duplicate_fields(&blocks, &lines, sink);
    }
    if repeated_params || classless_fields {
        for block in &blocks {
            let groups = block.bind_groups(source, &lines);
            if repeated_params {
                duplicate_params(block, &groups, &lines, sink);
            }
            if classless_fields {
                classless_fields_of(block, &groups, sink);
            }
        }
    }
    if operators {
        unknown_operators(&blocks, sink);
    }
    if casts {
        unknown_cast_variables(input.resolution, &blocks, sink);
    }
}

/// One doc comment: its `---` lines without the prefix, and what they declare.
struct DocBlock<'a> {
    comments: Vec<&'a Comment>,
    lines: Vec<&'a str>,
    doc: DocGroup,
}

impl<'a> DocBlock<'a> {
    fn new(source: &'a str, comments: Vec<&'a Comment>) -> Self {
        let lines: Vec<&str> = comments.iter().map(|comment| &comment.span.text(source)[3..]).collect();
        let doc = parse_doc_lines(&lines);
        Self { comments, lines, doc }
    }

    fn tag(&self, index: usize) -> Option<&'a str> {
        self.lines[index].trim_start().strip_prefix('@')?.split_whitespace().next()
    }

    /// The name line `index` declares, and where it is.
    fn declared(&self, index: usize) -> Option<(Span, &'a str)> {
        let (offset, name) = declared_name(self.lines.get(index)?)?;
        Some((self.span(index, offset, name), name))
    }

    /// Where `text`, which starts at byte `offset` of line `index`, is written.
    fn span(&self, index: usize, offset: usize, text: &str) -> Span {
        let start = self.comments[index].span.start + 3 + offset as u32;
        Span::new(start, start + text.len() as u32)
    }

    /// What line `index` is to lua-language-server.
    fn kind(&self, index: usize) -> LineKind<'a> {
        if self.lines[index].trim_start().starts_with('|') {
            return LineKind::Value;
        }
        match self.tag(index) {
            Some(tag) if LUA_LS_TAGS.contains(&tag) => LineKind::Tag(tag),
            _ => LineKind::Comment,
        }
    }

    /// The lines of the comment in the groups lua-language-server binds together, which decide what
    /// a `@field` or `@param` belongs to. A line joins the group of the line above when the two tags
    /// allow it: after a `@class`, only fields, operators, overloads and descriptions go on. A
    /// comment after code on its line is a group of its own, unless it is a field.
    fn bind_groups(&self, source: &str, lines: &LineIndex) -> Vec<Range<usize>> {
        let mut groups = Vec::new();
        let mut start = 0;
        let mut last: Option<(LineKind, u32)> = None;
        for index in 0..self.lines.len() {
            let kind = self.kind(index);
            let line = lines.line_of(self.comments[index].span.start);
            // A `---|` line lists another value of the line above, as part of it.
            if kind == LineKind::Value {
                if let Some((_, last_line)) = last.as_mut() {
                    *last_line = line;
                }
                continue;
            }
            if let Some((last_kind, last_line)) = last {
                // LuaLS reads the other comments in between, which a doc comment passes over, as
                // descriptions, which anything may follow.
                let last_kind = if line == last_line + 1 { last_kind } else { LineKind::Comment };
                if !continues(last_kind, kind) {
                    groups.push(start..index);
                    start = index;
                }
            }
            last = Some((kind, line));
            if index == 0 && !starts_line(source, self.comments[0].span.start) && kind != LineKind::Tag("field") {
                groups.push(0..1);
                (start, last) = (1, None);
            }
        }
        if start < self.lines.len() {
            groups.push(start..self.lines.len());
        }
        groups
    }
}

/// The tags lua-language-server reads; a line with any other is a description to it.
const LUA_LS_TAGS: &[&str] = &[
    "class",
    "type",
    "alias",
    "param",
    "return",
    "field",
    "generic",
    "vararg",
    "overload",
    "deprecated",
    "meta",
    "version",
    "see",
    "diagnostic",
    "module",
    "async",
    "nodiscard",
    "as",
    "cast",
    "operator",
    "source",
    "enum",
    "private",
    "protected",
    "public",
    "package",
];

/// What a doc line is to lua-language-server when it groups the lines of a comment.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LineKind<'a> {
    /// A line without a tag, or with one lua-language-server does not read, such as `@author`.
    Comment,
    /// A `---|` line, which lists another value of the line above.
    Value,
    Tag(&'a str),
}

/// Whether lua-language-server binds the line `next` to the group of the line `last` above it.
fn continues(last: LineKind, next: LineKind) -> bool {
    if next == LineKind::Tag("diagnostic") {
        return true;
    }
    let allowed = match last {
        LineKind::Tag("type" | "module" | "enum") => next == LineKind::Comment,
        LineKind::Tag("class" | "field" | "operator") => {
            matches!(next, LineKind::Comment | LineKind::Tag("field" | "operator" | "overload" | "source"))
        }
        _ => true,
    };
    allowed && next != LineKind::Tag("cast")
}

fn line_number(lines: &LineIndex, span: Span) -> u32 {
    lines.line_of(span.start) + 1
}

/// `undefined-doc-param`. A doc comment documents the functions of the statement below it, as
/// qbx-lua-ls binds it, with the functions it passes by name, and every function whose parameters
/// start on the line below it, as LuaLS does, which covers table fields and nested calls. Comment
/// lines in between do not separate them; a blank line does. A `@type fun(...)` in the comment
/// documents its parameter names too.
fn undefined_params<'a, 'c>(
    input: &FileInput<'a>,
    blocks: &[DocBlock],
    code: impl Fn() -> &'c Code<'a>,
    lines: &LineIndex,
    sink: &mut Sink,
) where
    'a: 'c,
{
    for block in blocks {
        // A doc comment after code on its line describes that line; only the lines below it document
        // the code that follows.
        let first = usize::from(!starts_line(input.source, block.comments[0].span.start));
        let params: Vec<(Span, &str)> = (first..block.lines.len())
            .filter(|&i| block.tag(i) == Some("param"))
            .filter_map(|i| block.declared(i))
            .collect();
        if params.is_empty() {
            continue;
        }
        let typed = (first..block.lines.len())
            .find(|&i| block.tag(i) == Some("type"))
            .and_then(|i| parse_doc_lines(&block.lines[i..=i]).ty);
        let last = block.comments[block.comments.len() - 1];
        let mut documented = documented_offset(input.source, &input.chunk.comments, last)
            .map(|offset| code().documented(offset, lines))
            .unwrap_or_default();
        if let Some(fun) = typed.as_ref().and_then(Type::as_fun) {
            documented.add_type(fun);
        }
        for (span, name) in params {
            if documented.has(name) {
                continue;
            }
            let message = if documented.functions == 0 {
                format!("no function follows the @param '{name}' annotation")
            } else {
                format!("the function below has no parameter '{name}'")
            };
            sink.report(rules::UNDEFINED_DOC_PARAM, span, message);
        }
    }
}

fn starts_line(source: &str, offset: u32) -> bool {
    source[..offset as usize].rsplit('\n').next().unwrap_or_default().trim().is_empty()
}

/// Where the code a doc comment documents starts: the first token after it, past any other comment
/// lines. `None` when a blank line or the end of the file comes first.
fn documented_offset(source: &str, comments: &[Comment], last: &Comment) -> Option<u32> {
    let mut cursor = last.span.end;
    let mut next = comments.partition_point(|comment| comment.span.start < cursor);
    loop {
        let rest = &source[cursor as usize..];
        let code = rest.trim_start();
        let gap = &rest[..rest.len() - code.len()];
        if code.is_empty() || gap.bytes().filter(|b| *b == b'\n').count() > 1 {
            return None;
        }
        let offset = cursor + gap.len() as u32;
        match comments.get(next) {
            Some(comment) if comment.span.start == offset => {
                cursor = comment.span.end;
                next += 1;
            }
            _ => return Some(offset),
        }
    }
}

/// The statements of a file by where they start, its functions by the line their parameters start
/// on, and the functions its variables are given where they are declared or assigned.
struct Code<'a> {
    resolution: &'a Resolution,
    statements: FxHashMap<u32, &'a Stmt>,
    functions: FxHashMap<u32, Vec<&'a FuncBody>>,
    named: FxHashMap<Variable<'a>, Vec<&'a FuncBody>>,
}

/// A local or global variable.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Variable<'a> {
    Local(LocalId),
    Global(&'a str),
}

impl<'a> Code<'a> {
    fn of(chunk: &'a Chunk, resolution: &'a Resolution, lines: &LineIndex) -> Self {
        let code = Code {
            resolution,
            statements: FxHashMap::default(),
            functions: FxHashMap::default(),
            named: FxHashMap::default(),
        };
        let mut collector = CodeCollector { code, lines };
        collector.visit_block(&chunk.block);
        collector.code
    }

    fn variable(&self, name: &'a Name) -> Option<Variable<'a>> {
        match self.resolution.resolve_at(name.span.start)? {
            Resolved::Local(id) => Some(Variable::Local(id)),
            Resolved::Global(_) => Some(Variable::Global(&name.text)),
        }
    }

    /// The parameters of the code at `offset`.
    fn documented(&self, offset: u32, lines: &LineIndex) -> Documented<'a> {
        let mut documented = Documented::default();
        if let Some(stmt) = self.statements.get(&offset) {
            match &stmt.kind {
                StmtKind::Function { name, .. } if name.method.is_some() => {
                    documented.names.insert("self");
                }
                // LuaLS lets `@param` type the variables of a `for ... in` loop.
                StmtKind::GenericFor { names, .. } => documented.names.extend(names.iter().map(|n| n.text.as_str())),
                _ => {}
            }
        }
        for func in self.bound(offset, lines, true) {
            documented.add(func);
        }
        documented
    }

    /// The functions a comment above the code at `offset` describes: those the statement there
    /// defines or passes, with `by_name` also those it passes by the name of a variable it gave
    /// them, and every function whose parameters start on its line.
    fn bound(&self, offset: u32, lines: &LineIndex, by_name: bool) -> Vec<&'a FuncBody> {
        let mut statement = StatementFunctions { functions: Vec::new(), code: self, by_name };
        if let Some(stmt) = self.statements.get(&offset) {
            statement.visit_stmt(stmt);
        }
        let mut functions = statement.functions;
        functions.extend(self.functions.get(&lines.line_of(offset)).into_iter().flatten());
        functions
    }

    /// The one function a doc comment above the code at `offset` annotates, as LuaLS binds `@param`
    /// and `@return`: the first one whose parameters start on its line, or else, for a statement
    /// that spans lines, the first one it defines or passes.
    fn annotated(&self, offset: u32, lines: &LineIndex) -> Option<&'a FuncBody> {
        if let Some(func) = self.functions.get(&lines.line_of(offset)).and_then(|functions| functions.first()) {
            return Some(func);
        }
        let mut statement = StatementFunctions { functions: Vec::new(), code: self, by_name: false };
        statement.visit_stmt(self.statements.get(&offset)?);
        statement.functions.first().copied()
    }
}

struct CodeCollector<'a, 'l> {
    code: Code<'a>,
    lines: &'l LineIndex,
}

impl<'a> CodeCollector<'a, '_> {
    fn name(&mut self, name: &'a Name, func: &'a FuncBody) {
        if let Some(variable) = self.code.variable(name) {
            self.code.named.entry(variable).or_default().push(func);
        }
    }
}

impl<'ast> Visitor<'ast> for CodeCollector<'ast, '_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        self.code.statements.insert(stmt.span.start, stmt);
        match &stmt.kind {
            StmtKind::LocalFunction { name, func } => self.name(name, func),
            StmtKind::Function { name, func } if name.path.is_empty() && name.method.is_none() => {
                self.name(&name.base, func)
            }
            StmtKind::Local { names, exprs, .. } => {
                for (name, expr) in names.iter().zip(exprs) {
                    if let ExprKind::Function(func) = &expr.kind {
                        self.name(&name.name, func);
                    }
                }
            }
            StmtKind::Assign { targets, exprs } => {
                for (target, expr) in targets.iter().zip(exprs) {
                    if let (ExprKind::Name(name), ExprKind::Function(func)) = (&target.kind, &expr.kind) {
                        self.name(name, func);
                    }
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_func_body(&mut self, func: &'ast FuncBody) {
        self.code.functions.entry(self.lines.line_of(func.params_span.start)).or_default().push(func);
        visit::walk_func_body(self, func);
    }
}

/// The functions a statement defines or passes, with `by_name` also by the name of a variable it
/// gave them, outside the blocks it runs and the bodies of those functions.
struct StatementFunctions<'a, 'd> {
    functions: Vec<&'a FuncBody>,
    code: &'d Code<'a>,
    by_name: bool,
}

impl<'ast> Visitor<'ast> for StatementFunctions<'ast, '_> {
    fn visit_block(&mut self, _: &'ast Block) {}

    fn visit_func_body(&mut self, func: &'ast FuncBody) {
        self.functions.push(func);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let (true, ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. }) = (self.by_name, &expr.kind) {
            for arg in args {
                let ExprKind::Name(name) = &arg.kind else { continue };
                let named = self.code.variable(name).and_then(|variable| self.code.named.get(&variable));
                self.functions.extend(named.into_iter().flatten());
            }
        }
        visit::walk_expr(self, expr);
    }
}

#[derive(Default)]
struct Documented<'a> {
    names: FxHashSet<&'a str>,
    vararg: bool,
    functions: usize,
}

impl<'a> Documented<'a> {
    fn add(&mut self, func: &'a FuncBody) {
        self.functions += 1;
        self.names.extend(func.params.iter().map(|param| param.text.as_str()));
        self.vararg |= func.vararg.is_some();
    }

    /// A `@type fun(...)` beside the `@param` lines, which types what the statement below assigns.
    fn add_type(&mut self, fun: &'a FunType) {
        self.functions += 1;
        for param in &fun.params {
            match param.name.as_str() {
                "..." => self.vararg = true,
                name => {
                    self.names.insert(name);
                }
            }
        }
    }

    fn has(&self, name: &str) -> bool {
        match name {
            "..." => self.vararg,
            _ => self.names.contains(name),
        }
    }
}

/// `missing-global-doc`, `missing-local-export-doc` and `incomplete-signature-doc`, as LuaLS reports
/// them: each parameter of a function that no `@param` names, and each value of each `return` at a
/// position no `@return` covers. A global or exported function without parameters or returned
/// values needs a comment instead. A `@type fun(...)` in the doc comment documents the parameters
/// and values it lists, and so does the function type a call declares for the function it is
/// passed, when the file or its resource documents the function called.
fn missing_docs<'a>(input: &FileInput<'a>, blocks: &[DocBlock], code: &Code<'a>, lines: &LineIndex, sink: &mut Sink) {
    let source = input.source;
    let mut signatures: FxHashMap<u32, (&FuncBody, Signature)> = FxHashMap::default();
    for block in blocks {
        let first = usize::from(!starts_line(source, block.comments[0].span.start));
        let last = block.comments[block.comments.len() - 1];
        let Some(offset) = documented_offset(source, &input.chunk.comments, last) else { continue };
        let Some(func) = code.annotated(offset, lines) else { continue };
        let signature = Signature::of(block, first);
        signatures.entry(func.params_span.start).or_insert_with(|| (func, Signature::default())).1.merge(&signature);
    }
    let gaps = Gaps { input, arguments: OnceCell::new(), callees: OnceCell::new() };
    if sink.enabled(rules::INCOMPLETE_SIGNATURE_DOC).is_some() {
        for (func, signature) in signatures.values().filter(|(_, signature)| signature.annotated) {
            for (span, gap) in gaps.of(func, &returned_values(func), signature) {
                let message = match gap {
                    Gap::Param(name) => format!("incomplete signature: parameter '{name}' has no @param annotation"),
                    Gap::Return(index) => {
                        format!("incomplete signature: return value #{index} has no @return annotation")
                    }
                };
                sink.report(rules::INCOMPLETE_SIGNATURE_DOC, span, message);
            }
        }
    }
    let globals = sink.enabled(rules::MISSING_GLOBAL_DOC).is_some();
    let exported = sink.enabled(rules::MISSING_LOCAL_EXPORT_DOC).is_some();
    if !(globals || exported) {
        return;
    }
    let mut targets = Targets::new(input.resolution);
    targets.visit_block(&input.chunk.block);
    let undocumented = Signature::default();
    let commented = OnceCell::new();
    let mut check = |rule: &'static str, subject: Subject, name: Span, func: &FuncBody| {
        let returns = returned_values(func);
        let signature = signatures.get(&func.params_span.start).map_or(&undocumented, |(_, signature)| signature);
        // LuaLS asks for a comment only here, where there is nothing to annotate.
        let bare = func.params.is_empty() && func.vararg.is_none() && returns.iter().all(|values| values.is_empty());
        if bare && !commented.get_or_init(|| commented_functions(input, code, lines)).contains(&func.params_span.start)
        {
            sink.report(rule, name, format!("{subject} has no comment"));
        }
        for (span, gap) in gaps.of(func, &returns, signature) {
            let message = match gap {
                Gap::Param(name) => format!("parameter '{name}' of {subject} has no @param annotation"),
                Gap::Return(index) => format!("return value #{index} of {subject} has no @return annotation"),
            };
            sink.report(rule, span, message);
        }
    };
    if globals {
        for &(name, func) in &targets.globals {
            check(rules::MISSING_GLOBAL_DOC, Subject::Global(&name.text), name.span, func);
        }
    }
    if exported {
        for (subject, name, func) in targets.exported() {
            check(rules::MISSING_LOCAL_EXPORT_DOC, subject, name, func);
        }
    }
}

/// What the doc comments of one function say about its parameters and returned values.
#[derive(Default)]
struct Signature {
    params: FxHashSet<SmolStr>,
    vararg: bool,
    returns: usize,
    /// The last `@return` is `...T`, which covers every later value.
    variadic_return: bool,
    /// An `@param` or `@return` annotates the function, so `incomplete-signature-doc` checks it.
    annotated: bool,
}

impl Signature {
    /// What the lines of `block` from `first` on say.
    fn of(block: &DocBlock, first: usize) -> Self {
        let parsed;
        let doc = if first == 0 {
            &block.doc
        } else {
            parsed = parse_doc_lines(&block.lines[first..]);
            &parsed
        };
        let mut signature = Signature::default();
        for param in &doc.params {
            signature.add_param(&param.name);
        }
        signature.add_returns(doc.returns.iter().map(|value| &value.ty));
        if let Some(fun) = doc.ty.as_ref().and_then(Type::as_fun) {
            for param in &fun.params {
                signature.add_param(&param.name);
            }
            signature.add_returns(fun.returns.iter());
        }
        signature.annotated = (first..block.lines.len()).any(|i| matches!(block.tag(i), Some("param" | "return")));
        signature
    }

    fn add_param(&mut self, name: &SmolStr) {
        match name.as_str() {
            "..." => self.vararg = true,
            _ => {
                self.params.insert(name.clone());
            }
        }
    }

    fn add_returns<'t>(&mut self, types: impl Iterator<Item = &'t Type>) {
        let types: Vec<&Type> = types.collect();
        self.returns = self.returns.max(types.len());
        self.variadic_return |= matches!(types.last(), Some(Type::Variadic(_)));
    }

    fn merge(&mut self, other: &Signature) {
        self.params.extend(other.params.iter().cloned());
        self.vararg |= other.vararg;
        self.returns = self.returns.max(other.returns);
        self.variadic_return |= other.variadic_return;
        self.annotated |= other.annotated;
    }

    /// Whether an annotation covers the returned value at `index`, counting from 1.
    fn covers_return(&self, index: usize) -> bool {
        index <= self.returns || self.variadic_return
    }
}

/// A parameter or returned value that no annotation documents.
enum Gap<'a> {
    Param(&'a str),
    /// The position of the value, counting from 1.
    Return(usize),
}

/// Finds what the annotations of a function leave out.
struct Gaps<'i, 'a> {
    input: &'i FileInput<'a>,
    arguments: OnceCell<FxHashMap<u32, (&'a Expr, usize)>>,
    callees: OnceCell<Callees<'i, 'a>>,
}

impl<'a> Gaps<'_, 'a> {
    /// The parameters of `func` that `signature` does not name, other than `self`, placeholders and
    /// those the function type it is passed as names, and the values of its `returns` at positions
    /// `signature` does not cover.
    fn of<'f>(&self, func: &'f FuncBody, returns: &[&'f [Expr]], signature: &Signature) -> Vec<(Span, Gap<'f>)> {
        let prefix = self.input.config.ignore_unused_prefix.as_str();
        let passed_as = OnceCell::new();
        let typed = |position: usize| {
            let functions = passed_as.get_or_init(|| self.passed_as(func));
            functions.iter().any(|fun| fun.params.get(position).is_some_and(|param| param.name != "..."))
        };
        let mut gaps = Vec::new();
        for (position, param) in func.params.iter().enumerate() {
            let name = param.text.as_str();
            let placeholder = name == "_" || (!prefix.is_empty() && name.starts_with(prefix));
            if !(name.is_empty() || name == "self" || placeholder || signature.params.contains(name) || typed(position))
            {
                gaps.push((param.span, Gap::Param(name)));
            }
        }
        if let Some(span) = func.vararg.filter(|_| !signature.vararg) {
            gaps.push((span, Gap::Param("...")));
        }
        for values in returns {
            for (index, value) in values.iter().enumerate() {
                if !signature.covers_return(index + 1) {
                    gaps.push((value.span, Gap::Return(index + 1)));
                }
            }
        }
        gaps
    }

    /// The function types that the call `func` is passed to declares for it, such as the
    /// `fun(source: number, ...)` of a callback wrapper.
    fn passed_as(&self, func: &FuncBody) -> Vec<Arc<FunType>> {
        let arguments = self.arguments.get_or_init(|| call_arguments(self.input.chunk));
        let Some(&(call, position)) = arguments.get(&func.params_span.start) else { return Vec::new() };
        self.callees.get_or_init(|| Callees::new(self.input)).argument_functions(call, position)
    }
}

/// The functions that calls pass, by where their parameters start, with the call and the position
/// of the argument.
fn call_arguments(chunk: &Chunk) -> FxHashMap<u32, (&Expr, usize)> {
    struct Arguments<'a>(FxHashMap<u32, (&'a Expr, usize)>);

    impl<'ast> Visitor<'ast> for Arguments<'ast> {
        fn visit_expr(&mut self, expr: &'ast Expr) {
            if let ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } = &expr.kind {
                for (position, arg) in args.iter().enumerate() {
                    if let ExprKind::Function(func) = &arg.kind {
                        self.0.insert(func.params_span.start, (expr, position));
                    }
                }
            }
            visit::walk_expr(self, expr);
        }
    }

    let mut arguments = Arguments(FxHashMap::default());
    arguments.visit_block(&chunk.block);
    arguments.0
}

/// The values of each `return` of `func`, not of the functions inside it.
fn returned_values(func: &FuncBody) -> Vec<&[Expr]> {
    struct Returns<'a>(Vec<&'a [Expr]>);

    impl<'ast> Visitor<'ast> for Returns<'ast> {
        fn visit_stmt(&mut self, stmt: &'ast Stmt) {
            if let StmtKind::Return(values) = &stmt.kind {
                self.0.push(values);
            }
            visit::walk_stmt(self, stmt);
        }

        fn visit_func_body(&mut self, _: &'ast FuncBody) {}
    }

    let mut returns = Returns(Vec::new());
    returns.visit_block(&func.body);
    returns.0
}

/// The functions with a comment, by where their parameters start, as LuaLS reads one: a comment of
/// any kind directly above the code that defines or passes them, or after code on the line their
/// parameters start on. A `---@diagnostic` line is no comment.
fn commented_functions(input: &FileInput, code: &Code, lines: &LineIndex) -> FxHashSet<u32> {
    let source = input.source;
    let mut commented = FxHashSet::default();
    for comment in &input.chunk.comments {
        let text = comment.span.text(source);
        if text.strip_prefix("---").is_some_and(|doc| doc.trim_start().starts_with("@diagnostic")) {
            continue;
        }
        let functions = if starts_line(source, comment.span.start) {
            match documented_offset(source, &input.chunk.comments, comment) {
                Some(offset) => code.bound(offset, lines, false),
                None => continue,
            }
        } else {
            code.functions.get(&lines.line_of(comment.span.start)).cloned().unwrap_or_default()
        };
        commented.extend(functions.iter().map(|func| func.params_span.start));
    }
    commented
}

/// What a finding of `missing-global-doc` or `missing-local-export-doc` calls the function.
#[derive(Clone, Copy)]
enum Subject<'a> {
    Global(&'a str),
    ExportedLocal(&'a str),
    /// A function written in an `exports(name, function() end)` call, by the name of the export.
    Export(&'a str),
}

impl fmt::Display for Subject<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Subject::Global(name) => write!(f, "global function '{name}'"),
            Subject::ExportedLocal(name) => write!(f, "exported local function '{name}'"),
            Subject::Export(name) => write!(f, "exported function '{name}'"),
        }
    }
}

/// The global functions of a file, and the local functions it exports: those assigned by name to
/// fields of a local table that a `return` gives, as LuaLS reads a module, and those passed to
/// `exports(name, fn)`, which also exports a function written in the call.
struct Targets<'a> {
    resolution: &'a Resolution,
    globals: Vec<(&'a Name, &'a FuncBody)>,
    /// Locals declared with a function, by the name that declares them.
    local_functions: FxHashMap<LocalId, (&'a Name, &'a FuncBody)>,
    /// Locals declared with a table constructor.
    tables: FxHashSet<LocalId>,
    returned: FxHashSet<LocalId>,
    /// `table.field = local` assignments, as the table and the local assigned.
    fields: Vec<(LocalId, LocalId)>,
    exported_locals: Vec<LocalId>,
    exported_functions: Vec<(&'a str, Span, &'a FuncBody)>,
}

impl<'a> Targets<'a> {
    fn new(resolution: &'a Resolution) -> Self {
        Self {
            resolution,
            globals: Vec::new(),
            local_functions: FxHashMap::default(),
            tables: FxHashSet::default(),
            returned: FxHashSet::default(),
            fields: Vec::new(),
            exported_locals: Vec::new(),
            exported_functions: Vec::new(),
        }
    }

    fn local(&self, name: &Name) -> Option<LocalId> {
        match self.resolution.resolve_at(name.span.start)? {
            Resolved::Local(id) => Some(id),
            Resolved::Global(_) => None,
        }
    }

    fn is_global(&self, name: &Name) -> bool {
        matches!(self.resolution.resolve_at(name.span.start), Some(Resolved::Global(_)))
    }

    /// The exported functions, each once, with the name that defines them.
    fn exported(&self) -> Vec<(Subject<'a>, Span, &'a FuncBody)> {
        let module =
            self.fields.iter().filter(|(table, _)| self.tables.contains(table) && self.returned.contains(table));
        let locals = module.map(|&(_, local)| local).chain(self.exported_locals.iter().copied());
        let mut seen = FxHashSet::default();
        let mut exported: Vec<_> = locals
            .filter(|local| seen.insert(*local))
            .filter_map(|local| self.local_functions.get(&local))
            .map(|&(name, func)| (Subject::ExportedLocal(&name.text), name.span, func))
            .collect();
        exported.extend(self.exported_functions.iter().map(|&(name, span, func)| (Subject::Export(name), span, func)));
        exported
    }
}

impl<'ast> Visitor<'ast> for Targets<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::Function { name, func } if name.path.is_empty() && name.method.is_none() => {
                if self.is_global(&name.base) {
                    self.globals.push((&name.base, func));
                }
            }
            StmtKind::LocalFunction { name, func } => {
                if let Some(local) = self.local(name) {
                    self.local_functions.insert(local, (name, func));
                }
            }
            StmtKind::Local { names, exprs, .. } => {
                for (name, expr) in names.iter().zip(exprs) {
                    let Some(local) = self.local(&name.name) else { continue };
                    match &expr.kind {
                        ExprKind::Function(func) => {
                            self.local_functions.insert(local, (&name.name, func));
                        }
                        ExprKind::Table(_) => {
                            self.tables.insert(local);
                        }
                        _ => {}
                    }
                }
            }
            StmtKind::Assign { targets, exprs } => {
                for (target, expr) in targets.iter().zip(exprs) {
                    match (&target.kind, &expr.kind) {
                        (ExprKind::Name(name), ExprKind::Function(func)) if self.is_global(name) => {
                            self.globals.push((name, func));
                        }
                        (ExprKind::Field { base, .. }, ExprKind::Name(value)) => {
                            let ExprKind::Name(table) = &base.kind else { continue };
                            if let (Some(table), Some(value)) = (self.local(table), self.local(value)) {
                                self.fields.push((table, value));
                            }
                        }
                        _ => {}
                    }
                }
            }
            StmtKind::Return(values) => {
                for value in values {
                    if let Some(local) = name_of(value).and_then(|name| self.local(name)) {
                        self.returned.insert(local);
                    }
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let ExprKind::Call { callee, args, .. } = &expr.kind {
            let exports = name_of(callee).is_some_and(|callee| callee.text == "exports" && self.is_global(callee));
            if let (true, [name, value, ..]) = (exports, args.as_slice()) {
                match (name.as_string(), &value.kind) {
                    (Some(export), ExprKind::Function(func)) => self.exported_functions.push((export, name.span, func)),
                    (Some(_), ExprKind::Name(local)) => self.exported_locals.extend(self.local(local)),
                    _ => {}
                }
            }
        }
        visit::walk_expr(self, expr);
    }
}

fn name_of(expr: &Expr) -> Option<&Name> {
    match &expr.kind {
        ExprKind::Name(name) => Some(name),
        _ => None,
    }
}

/// A type name a doc comment declares.
struct TypeName<'a> {
    name: &'a str,
    /// What declares it, as the message names it.
    kind: &'static str,
    side: Option<Side>,
    partial: bool,
    span: Span,
}

const CLASS: &str = "a class";

/// `duplicate-doc-alias`: an `@alias` or `@enum` whose name the file already gives an alias or enum,
/// or any class, for a side the two share. `(partial)` on any of them allows the repeat, as in LuaLS.
fn duplicate_aliases(blocks: &[DocBlock], lines: &LineIndex, sink: &mut Sink) {
    let mut names: Vec<TypeName> = Vec::new();
    for block in blocks {
        for index in 0..block.lines.len() {
            let (kind, side) = match block.tag(index) {
                Some("alias") => ("an alias", block.doc.aliases.iter().find(|a| a.line == index).and_then(|a| a.side)),
                Some("enum") => ("an enum", block.doc.enum_side),
                Some("class") => (CLASS, block.doc.classes.iter().find(|c| c.line == index).and_then(|c| c.side)),
                _ => continue,
            };
            let Some((span, name)) = block.declared(index) else { continue };
            let partial = has_attribute(block.lines[index], "partial");
            names.push(TypeName { name, kind, side, partial, span });
        }
    }
    let partial: FxHashSet<&str> = names.iter().filter(|n| n.partial).map(|n| n.name).collect();
    for (index, declared) in names.iter().enumerate() {
        if declared.kind == CLASS || partial.contains(declared.name) {
            continue;
        }
        let clashes = |other: &&TypeName| other.name == declared.name && applies_on(other.side, declared.side);
        let earlier = names[..index].iter().rev().filter(|other| other.kind != CLASS).find(clashes);
        let Some(other) = earlier.or_else(|| names.iter().filter(|other| other.kind == CLASS).find(clashes)) else {
            continue;
        };
        let message = format!(
            "'{}' is already declared as {} on line {}",
            declared.name,
            other.kind,
            line_number(lines, other.span)
        );
        sink.report(rules::DUPLICATE_DOC_ALIAS, declared.span, message);
    }
}

/// The declarations of one field of a class, with the side each is for.
type FieldDeclarations = Vec<(Option<Side>, Span)>;

/// `duplicate-doc-field`: a class field the file already declares for a side the two share. A
/// function field repeated under its name is another signature of it, as in LuaLS.
fn duplicate_fields(blocks: &[DocBlock], lines: &LineIndex, sink: &mut Sink) {
    let mut seen: FxHashMap<(SmolStr, String), FieldDeclarations> = FxHashMap::default();
    for block in blocks {
        for class in &block.doc.classes {
            let named = class.fields.iter().map(|f| (f.name.to_string(), &f.ty, f.side, f.line));
            let keyed = class.indices.iter().chain(&class.literal_fields);
            let keyed = keyed.map(|f| (format!("[{}]", f.key), &f.ty, f.side, f.line));
            for (key, ty, side, index) in named.chain(keyed) {
                if matches!(ty, Type::Fun(_)) {
                    continue;
                }
                let Some((span, shown)) = block.declared(index) else { continue };
                let side = side.or(class.side);
                let declared = seen.entry((class.name.clone(), key)).or_default();
                if let Some((_, first)) = declared.iter().find(|(other, _)| applies_on(*other, side)) {
                    let message = format!(
                        "field '{shown}' of class '{}' is already declared on line {}",
                        class.name,
                        line_number(lines, *first)
                    );
                    sink.report(rules::DUPLICATE_DOC_FIELD, span, message);
                }
                declared.push((side, span));
            }
        }
    }
}

/// `duplicate-doc-param`: two `@param` lines for one name in a group of doc lines, as
/// lua-language-server binds them. Each is reported, with the line of another.
fn duplicate_params(block: &DocBlock, groups: &[Range<usize>], lines: &LineIndex, sink: &mut Sink) {
    for group in groups {
        let params: Vec<(Span, &str)> =
            group.clone().filter(|&i| block.tag(i) == Some("param")).filter_map(|i| block.declared(i)).collect();
        for (span, name) in &params {
            let Some((other, _)) = params.iter().find(|(other, other_name)| other != span && other_name == name) else {
                continue;
            };
            let message = format!("duplicate @param '{name}', also on line {}", line_number(lines, *other));
            sink.report(rules::DUPLICATE_DOC_PARAM, *span, message);
        }
    }
}

/// `doc-field-no-class`: a `@field` without a `@class` before it in its group of doc lines, which
/// lua-language-server then gives to no class. qbx-lua-ls still gives it to a `@class` higher up in
/// the comment, so the message names what separates the two.
fn classless_fields_of(block: &DocBlock, groups: &[Range<usize>], sink: &mut Sink) {
    for group in groups {
        let class = group.clone().position(|i| block.tag(i) == Some("class")).map(|position| group.start + position);
        for index in group.clone().filter(|&i| block.tag(i) == Some("field")) {
            if class.is_some_and(|class| class < index) {
                continue;
            }
            let Some((span, name)) = block.declared(index) else { continue };
            let message = match (0..group.start).any(|i| block.tag(i) == Some("class")) {
                false => format!("field '{name}' has no @class above it"),
                true => match separator(block, group.start) {
                    Some(separator) => format!("{separator} separates field '{name}' from its @class"),
                    None => format!("field '{name}' does not directly follow its @class"),
                },
            };
            sink.report(rules::DOC_FIELD_NO_CLASS, span, message);
        }
    }
}

/// The line that starts a new group of doc lines at line `start` of `block`, as a message names it:
/// a tag that may not follow a class or its fields, like `@deprecated`, or one that nothing may
/// follow, like `@type`.
fn separator(block: &DocBlock, start: usize) -> Option<String> {
    if let LineKind::Tag(tag) = block.kind(start) {
        if tag != "field" {
            return Some(format!("the @{tag} line"));
        }
    }
    let above = start.checked_sub(1)?;
    block.tag(above).filter(|tag| *tag != "class").map(|tag| format!("the @{tag} line"))
}

/// The operators an `@operator` line can declare: the metamethods of Lua 5.4, without their `__`,
/// that lua-language-server reads. It also knows LuaJIT's `sar`, which is accepted unlisted.
const OPERATORS: &[&str] = &[
    "add", "sub", "mul", "div", "mod", "pow", "idiv", "band", "bor", "bxor", "shl", "shr", "concat", "unm", "bnot",
    "len", "call",
];

/// `unknown-operator`: an `@operator` whose name is no operator lua-language-server knows, such as
/// `eq` or `index`, which it cannot apply.
fn unknown_operators(blocks: &[DocBlock], sink: &mut Sink) {
    for block in blocks {
        for index in (0..block.lines.len()).filter(|&i| block.tag(i) == Some("operator")) {
            let line = block.lines[index];
            let Some(rest) = line.trim_start().strip_prefix("@operator") else { continue };
            let rest = rest.trim_start();
            let name = &rest[..rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len())];
            if name.is_empty() || name == "sar" || OPERATORS.contains(&name) {
                continue;
            }
            let (listed, last) = OPERATORS.split_at(OPERATORS.len() - 1);
            let message = format!("unknown operator '{name}'; @operator takes {} or {}", listed.join(", "), last[0]);
            sink.report(rules::UNKNOWN_OPERATOR, block.span(index, line.len() - rest.len(), name), message);
        }
    }
}

/// `unknown-cast-variable`: a `---@cast` whose name is no local in scope where it is written, such as
/// a global, a field or a local declared below it, whose type it cannot change.
fn unknown_cast_variables(resolution: &Resolution, blocks: &[DocBlock], sink: &mut Sink) {
    for block in blocks {
        for (index, line) in block.lines.iter().enumerate() {
            let Some(cast) = parse_cast(line) else { continue };
            let comment = block.comments[index];
            if resolution.lookup_local_at(cast.name, comment.span.start).is_some() {
                continue;
            }
            let Some(rest) = line.trim_start().strip_prefix("@cast") else { continue };
            let offset = line.len() - rest.trim_start().len();
            let message = format!("no local '{}' is in scope here; @cast changes the type of a local", cast.name);
            sink.report(rules::UNKNOWN_CAST_VARIABLE, block.span(index, offset, cast.name), message);
        }
    }
}
