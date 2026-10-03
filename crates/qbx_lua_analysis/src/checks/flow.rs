use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{LineIndex, NumberValue, SmolStr, Span};
use rustc_hash::FxHashMap;

use super::{FileInput, Sink};
use crate::diagnostic::{Fix, Tag, TextEdit};
use crate::directives::Suppressions;
use crate::env::is_meta_file;
use crate::rules;
use crate::scope::{LocalId, Resolved};

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    for label in input.resolution.labels.iter().filter(|l| !l.used) {
        sink.report_with(
            rules::UNUSED_LABEL,
            label.span,
            format!("label '{}' is never used", label.name),
            Some(Tag::Unnecessary),
            None,
        );
    }
    for name in &input.resolution.undefined_gotos {
        sink.report(rules::UNDEFINED_LABEL, name.span, format!("no visible label '{}' for goto", name.text));
    }
    // Definition files only declare signatures, which may repeat a field.
    let set_fields = sink.enabled(rules::DUPLICATE_SET_FIELD).is_some() && !is_meta_file(input.source, input.chunk);
    let mut flow = Flow {
        input,
        sink,
        block: 0,
        function: 0,
        blocks: 0,
        set_fields: set_fields.then(FxHashMap::default),
        lines: None,
        suppressions: None,
    };
    flow.visit_block(&input.chunk.block);
}

struct Flow<'a, 'b> {
    input: &'a FileInput<'a>,
    sink: &'a mut Sink<'b>,
    /// The block `duplicate-set-field` compares definitions in: the main chunk, a function body or
    /// one branch of an `if`, as LuaLS scopes the rule, and the code after an `if` that can return,
    /// which runs only when it does not. Loops and `do` blocks share the block around them.
    block: u32,
    /// The block of the function body the code is in.
    function: u32,
    blocks: u32,
    /// The functions set on table fields, by the variable and the keys they are set through. `None`
    /// when the rule is off.
    set_fields: Option<FxHashMap<Root, FxHashMap<String, Vec<SetField>>>>,
    lines: Option<LineIndex>,
    suppressions: Option<Suppressions>,
}

/// The variable a table field is set through.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Root {
    Local(LocalId),
    Global(SmolStr),
}

/// The last function set on a table field in one block.
struct SetField {
    block: u32,
    function: u32,
    span: Span,
}

impl Flow<'_, '_> {
    fn has_comment_inside(&self, span: Span) -> bool {
        let comments = &self.input.chunk.comments;
        let first = comments.partition_point(|c| c.span.start < span.start);
        comments.get(first).is_some_and(|c| c.span.end <= span.end)
    }

    fn empty_block(&mut self, block: &Block, region: Span, what: &str) {
        if block.stmts.is_empty() && !self.has_comment_inside(region) {
            self.sink.report(rules::EMPTY_BLOCK, region, format!("empty {what} block"));
        }
    }

    fn unreachable(&mut self, block: &Block) {
        let exit = block.stmts.iter().position(|s| matches!(s.kind, StmtKind::Break | StmtKind::Goto(_)));
        let Some(exit) = exit else { return };
        let Some(next) = block.stmts.get(exit + 1) else { return };
        if matches!(next.kind, StmtKind::Label(_)) {
            return;
        }
        let end = block.stmts.last().map_or(next.span.end, |s| s.span.end);
        self.sink.report_with(
            rules::UNREACHABLE_CODE,
            Span::new(next.span.start, end),
            "unreachable code",
            Some(Tag::Unnecessary),
            None,
        );
    }

    fn balance(&mut self, targets: usize, exprs: &[Expr], stmt_span: Span) {
        let Some(last) = exprs.last() else { return };
        if exprs.len() > targets {
            let extra = exprs[targets].span.to(last.span);
            self.sink.report(
                rules::UNBALANCED_ASSIGNMENTS,
                extra,
                format!("{} value(s) assigned to {targets} target(s); the extra values are discarded", exprs.len()),
            );
        } else if exprs.len() < targets && !last.is_multi_value() {
            self.sink.report(
                rules::UNBALANCED_ASSIGNMENTS,
                stmt_span,
                format!("{} value(s) assigned to {targets} target(s); the remaining targets become nil", exprs.len()),
            );
        }
    }

    fn duplicate_keys(&mut self, fields: &[TableField]) {
        let mut seen: FxHashMap<String, ()> = FxHashMap::default();
        for field in fields {
            let (key, span) = match field {
                TableField::Named { name, .. } => (format!("s:{}", name.text), name.span),
                TableField::SetMember(name) => (format!("s:{}", name.text), name.span),
                TableField::Keyed { key, .. } => match &key.kind {
                    ExprKind::String(s) => (format!("s:{s}"), key.span),
                    ExprKind::Number(NumberValue::Int(i)) => (format!("n:{i}"), key.span),
                    ExprKind::JenkinsHash(h) => (format!("h:{}", h.to_ascii_lowercase()), key.span),
                    _ => continue,
                },
                TableField::Positional(_) => continue,
            };
            if seen.insert(key.clone(), ()).is_some() {
                self.sink.report(rules::DUPLICATE_INDEX, span, format!("duplicate table key '{}'", &key[2..]));
            }
        }
    }

    /// Visits code that is a block of its own for `duplicate-set-field`.
    fn in_new_block(&mut self, visit: impl FnOnce(&mut Self)) {
        let outer = self.block;
        self.blocks += 1;
        self.block = self.blocks;
        visit(self);
        self.block = outer;
    }

    fn root(&self, name: &Name) -> Option<Root> {
        match self.input.resolution.resolve_at(name.span.start)? {
            Resolved::Local(id) => Some(Root::Local(id)),
            Resolved::Global(_) => Some(Root::Global(name.text.clone())),
        }
    }

    /// The variable an `a.b.c` or `a['b']` target starts from, and the keys after it.
    fn place(&self, expr: &Expr) -> Option<(Root, String)> {
        let (base, key) = match &expr.kind {
            ExprKind::Name(name) => return Some((self.root(name)?, String::new())),
            ExprKind::Paren(inner) => return self.place(inner),
            ExprKind::Field { base, name, .. } => (base, format!("s:{}", name.text)),
            ExprKind::Index { base, index, .. } => match &index.kind {
                ExprKind::String(key) => (base, format!("s:{key}")),
                ExprKind::Number(NumberValue::Int(key)) => (base, format!("n:{key}")),
                _ => return None,
            },
            _ => return None,
        };
        let (root, path) = self.place(base)?;
        Some((root, format!("{path}.{key}")))
    }

    /// `duplicate-set-field`: `function M.f()` or `M.f = function` where the same block already set
    /// a function on that field. Side guards need no check of their own: each side's definition is
    /// in a branch of its `if`, or after an `if` that returns on the other side.
    fn set_function(&mut self, root: Option<Root>, path: String, span: Span) {
        let (Some(root), Some(set_fields)) = (root, self.set_fields.as_mut()) else { return };
        let (block, function) = (self.block, self.function);
        let definitions = set_fields.entry(root).or_default().entry(path).or_default();
        let Some(earlier) = definitions.iter_mut().find(|definition| definition.block == block) else {
            definitions.push(SetField { block, function, span });
            return;
        };
        let earlier = std::mem::replace(&mut earlier.span, span);
        if self.suppressed_at(earlier.start) {
            return;
        }
        let line = self.line_index().line_of(earlier.start) + 1;
        let shown = span.text(self.input.source);
        self.sink.report(
            rules::DUPLICATE_SET_FIELD,
            span,
            format!("'{shown}' is already defined on line {line}; this definition replaces it"),
        );
    }

    fn has_set_fields(&self) -> bool {
        self.set_fields.as_ref().is_some_and(|set_fields| !set_fields.is_empty())
    }

    /// A read of a field, as `local old = M.f` is before wrapping it, so a later definition does not
    /// simply replace the one that was read. Reads in other functions run at another time.
    fn read_field(&mut self, place: Option<(Root, String)>) {
        let Some((root, path)) = place else { return };
        let function = self.function;
        let set_fields = self.set_fields.as_mut().and_then(|set_fields| set_fields.get_mut(&root));
        if let Some(definitions) = set_fields.and_then(|fields| fields.get_mut(&path)) {
            definitions.retain(|definition| definition.function != function);
        }
    }

    /// An assignment to a table, as `p = lib.points.new(b)` is, which leaves the functions set on the
    /// fields of the table it replaces behind.
    fn assign_table(&mut self, target: &Expr) {
        let Some((root, path)) = self.place(target) else { return };
        let function = self.function;
        let Some(fields) = self.set_fields.as_mut().and_then(|set_fields| set_fields.get_mut(&root)) else { return };
        for (field, definitions) in fields.iter_mut() {
            if field.strip_prefix(path.as_str()).is_some_and(|rest| rest.starts_with('.')) {
                definitions.retain(|definition| definition.function != function);
            }
        }
    }

    fn line_index(&mut self) -> &LineIndex {
        let source = self.input.source;
        self.lines.get_or_insert_with(|| LineIndex::new(source))
    }

    /// LuaLS leaves a repeated field alone when the rule is disabled at either definition.
    fn suppressed_at(&mut self, offset: u32) -> bool {
        let (source, comments) = (self.input.source, &self.input.chunk.comments);
        let lines = self.lines.get_or_insert_with(|| LineIndex::new(source));
        let suppressions = self.suppressions.get_or_insert_with(|| Suppressions::parse(source, comments, lines));
        suppressions.is_suppressed(rules::DUPLICATE_SET_FIELD, lines.line_of(offset))
    }

    /// `count-down-loop`: `for i = 10, 1` never runs, and `for i = #list, 1` never runs once its start
    /// is above 1. Both were meant to count down.
    fn count_down(&mut self, start: &Expr, limit: &Expr, step: Option<&Expr>) {
        let Some(last) = number(limit) else { return };
        if step.is_some_and(|step| !number(step).is_some_and(|step| step > 0.0)) {
            return;
        }
        let from_length = step.is_none() && last == 1.0 && starts_with_length(start);
        if !from_length && !number(start).is_some_and(|first| first > last) {
            return;
        }
        let source = self.input.source;
        let text = |expr: &Expr| expr.span.text(source).split_whitespace().collect::<Vec<_>>().join(" ");
        let (first, last) = (text(start), text(limit));
        let (edit, negative) = match step {
            Some(step) => {
                let negative = format!("-{}", text(step));
                (TextEdit { span: step.span, new_text: negative.clone() }, negative)
            }
            None => {
                (TextEdit { span: Span::new(limit.span.end, limit.span.end), new_text: ", -1".into() }, "-1".into())
            }
        };
        let message = if from_length {
            format!("the loop counts up from {first} to 1, so it never runs when {first} is greater than 1; did you mean `{first}, 1, -1`?")
        } else {
            format!(
                "the loop never runs: it counts up from {first} to {last}; did you mean `{first}, {last}, {negative}`?"
            )
        };
        let fix = Fix { title: format!("Count down with a step of {negative}"), edits: vec![edit] };
        let span = start.span.to(step.unwrap_or(limit).span);
        self.sink.report_with(rules::COUNT_DOWN_LOOP, span, message, None, Some(fix));
    }
}

/// The value of a numeric literal, negated or in parentheses.
fn number(expr: &Expr) -> Option<f64> {
    match &expr.unparen().kind {
        ExprKind::Number(NumberValue::Int(value)) => Some(*value as f64),
        ExprKind::Number(NumberValue::Float(value)) => Some(*value),
        ExprKind::Unary { op: UnOp::Neg, expr } => number(expr).map(|value| -value),
        _ => None,
    }
}

/// `#list`, or arithmetic that starts with it, like `#list - 1`.
fn starts_with_length(expr: &Expr) -> bool {
    match &expr.unparen().kind {
        ExprKind::Unary { op: UnOp::Len, .. } => true,
        ExprKind::Binary { op: BinOp::Add | BinOp::Sub, lhs, .. } => starts_with_length(lhs),
        _ => false,
    }
}

fn same_place(a: &Expr, b: &Expr) -> bool {
    match (a.unparen().dotted_path(), b.unparen().dotted_path()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// An `if` with a branch that ends in `return`.
fn can_return(stmt: &Stmt) -> bool {
    let StmtKind::If { branches, else_block } = &stmt.kind else { return false };
    let returns = |block: &Block| matches!(block.stmts.last(), Some(Stmt { kind: StmtKind::Return(_), .. }));
    branches.iter().map(|branch| &branch.block).chain(else_block).any(returns)
}

impl<'ast> Visitor<'ast> for Flow<'_, '_> {
    fn visit_block(&mut self, block: &'ast Block) {
        self.unreachable(block);
        let outer = self.block;
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
            if can_return(stmt) {
                self.blocks += 1;
                self.block = self.blocks;
            }
        }
        self.block = outer;
    }

    fn visit_func_body(&mut self, func: &'ast FuncBody) {
        for (i, param) in func.params.iter().enumerate() {
            if param.text != "_" && func.params[..i].iter().any(|p| p.text == param.text) {
                self.sink.report(rules::DUPLICATE_ARGUMENT, param.span, format!("duplicate argument '{}'", param.text));
            }
        }
        let outer = self.function;
        self.in_new_block(|flow| {
            flow.function = flow.block;
            visit::walk_func_body(flow, func);
        });
        self.function = outer;
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::Local { names, exprs, in_unpack: false } => self.balance(names.len(), exprs, stmt.span),
            StmtKind::Assign { targets, exprs } => {
                self.balance(targets.len(), exprs, stmt.span);
                if let ([target], [value]) = (targets.as_slice(), exprs.as_slice()) {
                    if same_place(target, value) {
                        self.sink.report(rules::SELF_ASSIGNMENT, stmt.span, "value is assigned to itself");
                    }
                }
                exprs.iter().for_each(|value| self.visit_expr(value));
                for (i, target) in targets.iter().enumerate() {
                    // What the target is reached through is read; the target itself is written.
                    match &target.kind {
                        ExprKind::Name(_) => {}
                        ExprKind::Field { base, .. } => self.visit_expr(base),
                        ExprKind::Index { base, index, .. } => {
                            self.visit_expr(base);
                            self.visit_expr(index);
                        }
                        _ => self.visit_expr(target),
                    }
                    if self.has_set_fields() {
                        self.assign_table(target);
                    }
                    if exprs.get(i).is_some_and(|value| matches!(value.kind, ExprKind::Function(_))) {
                        if let Some((root, path)) = self.place(target).filter(|(_, path)| !path.is_empty()) {
                            self.set_function(Some(root), path, target.span);
                        }
                    }
                }
                return;
            }
            StmtKind::Function { name, .. } if !name.path.is_empty() || name.method.is_some() => {
                let path = name.path.iter().chain(&name.method).map(|key| format!(".s:{}", key.text)).collect();
                self.set_function(self.root(&name.base), path, name.span);
            }
            StmtKind::Do(body) => self.empty_block(body, stmt.span, "do"),
            StmtKind::While { body, .. } => self.empty_block(body, stmt.span, "while"),
            StmtKind::Repeat { body, .. } => self.empty_block(body, stmt.span, "repeat"),
            StmtKind::NumericFor { start, limit, step, body, .. } => {
                self.empty_block(body, stmt.span, "for");
                self.count_down(start, limit, step.as_ref());
            }
            StmtKind::GenericFor { body, .. } => self.empty_block(body, stmt.span, "for"),
            StmtKind::If { branches, else_block } => {
                for (i, branch) in branches.iter().enumerate() {
                    let end = branches
                        .get(i + 1)
                        .map(|b| b.keyword_span.start)
                        .or(else_block.as_ref().map(|b| b.span.start.max(branch.block.span.end)))
                        .unwrap_or(stmt.span.end);
                    let region = Span::new(branch.keyword_span.start, end.max(branch.keyword_span.end));
                    let what = if i == 0 { "if" } else { "elseif" };
                    self.empty_block(&branch.block, region, what);
                }
                if let Some(block) = else_block {
                    let start = branches.last().map_or(stmt.span.start, |b| b.block.span.end.max(b.cond.span.end));
                    self.empty_block(block, Span::new(start, stmt.span.end), "else");
                }
                for branch in branches {
                    self.visit_expr(&branch.cond);
                    self.in_new_block(|flow| flow.visit_block(&branch.block));
                }
                if let Some(block) = else_block {
                    self.in_new_block(|flow| flow.visit_block(block));
                }
                return;
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match &expr.kind {
            ExprKind::Table(fields) => self.duplicate_keys(fields),
            ExprKind::Binary { op: BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, lhs, rhs, .. }
                if same_place(lhs, rhs) =>
            {
                self.sink.report(
                    rules::SELF_COMPARISON,
                    expr.span,
                    "both sides of the comparison are the same expression",
                );
            }
            ExprKind::Field { .. } | ExprKind::Index { .. } if self.has_set_fields() => {
                self.read_field(self.place(expr))
            }
            ExprKind::MethodCall { base, method, .. } if self.has_set_fields() => {
                let place = self.place(base).map(|(root, path)| (root, format!("{path}.s:{}", method.text)));
                self.read_field(place);
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}
