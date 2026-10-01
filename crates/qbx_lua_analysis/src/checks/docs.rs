//! Doc comment checks that need no other file: `@param` names that no parameter of the documented
//! function has, and aliases and enums that one file declares twice for one side.

use qbx_fivem_data::Side;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{Comment, LineIndex, Span};
use qbx_luacats::luacats::{applies_on, declared_name, has_attribute, parse_doc_lines, DocGroup};
use qbx_luacats::types::{FunType, Type};
use rustc_hash::{FxHashMap, FxHashSet};

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
    if !(params || aliases) {
        return;
    }
    let blocks: Vec<DocBlock> = doc_blocks(source, &input.chunk.comments)
        .into_iter()
        .filter(|comments| comments.iter().any(|comment| comment.span.text(source).contains('@')))
        .map(|comments| DocBlock::new(source, comments))
        .collect();
    let lines = LineIndex::new(source);
    if params {
        undefined_params(input, &blocks, &lines, sink);
    }
    if aliases {
        duplicate_aliases(&blocks, &lines, sink);
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
        let start = self.comments[index].span.start + 3 + offset as u32;
        Some((Span::new(start, start + name.len() as u32), name))
    }
}

fn line_number(lines: &LineIndex, span: Span) -> u32 {
    lines.line_of(span.start) + 1
}

/// `undefined-doc-param`. A doc comment documents the functions of the statement below it, as
/// qbx-lua-ls binds it, with the functions it passes by name, and every function whose parameters
/// start on the line below it, as LuaLS does, which covers table fields and nested calls. Comment
/// lines in between do not separate them; a blank line does. A `@type fun(...)` in the comment
/// documents its parameter names too.
fn undefined_params(input: &FileInput, blocks: &[DocBlock], lines: &LineIndex, sink: &mut Sink) {
    let mut code: Option<Code> = None;
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
        let code = code.get_or_insert_with(|| Code::of(input.chunk, input.resolution, lines));
        let last = block.comments[block.comments.len() - 1];
        let mut documented = documented_offset(input.source, &input.chunk.comments, last)
            .map(|offset| code.documented(offset, lines))
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
            StatementFunctions { documented: &mut documented, code: self }.visit_stmt(stmt);
        }
        for func in self.functions.get(&lines.line_of(offset)).into_iter().flatten() {
            documented.add(func);
        }
        documented
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

/// The functions a statement defines or passes, also by the name of a variable it gave them,
/// outside the blocks it runs and the bodies of those functions.
struct StatementFunctions<'a, 'd> {
    documented: &'d mut Documented<'a>,
    code: &'d Code<'a>,
}

impl<'ast> Visitor<'ast> for StatementFunctions<'ast, '_> {
    fn visit_block(&mut self, _: &'ast Block) {}

    fn visit_func_body(&mut self, func: &'ast FuncBody) {
        self.documented.add(func);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } = &expr.kind {
            for arg in args {
                let ExprKind::Name(name) = &arg.kind else { continue };
                let named = self.code.variable(name).and_then(|variable| self.code.named.get(&variable));
                for func in named.into_iter().flatten() {
                    self.documented.add(func);
                }
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
