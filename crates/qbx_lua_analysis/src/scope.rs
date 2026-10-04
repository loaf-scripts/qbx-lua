use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{SmolStr, Span};
use rustc_hash::FxHashMap;

pub type LocalId = u32;
pub type FuncId = u32;

pub const MAIN_CHUNK: FuncId = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    Local,
    LocalFunction,
    Param,
    ImplicitSelf,
    LoopVar,
}

#[derive(Clone, Copy, Debug)]
pub struct LocalRef {
    pub span: Span,
    pub write: bool,
    pub func: FuncId,
}

#[derive(Clone, Debug)]
pub struct Local {
    pub name: SmolStr,
    pub decl: Span,
    pub kind: LocalKind,
    pub attrib: Option<Attrib>,
    pub func: FuncId,
    pub visible_from: u32,
    pub scope_end: u32,
    pub shadows: Option<LocalId>,
    pub redefines: Option<LocalId>,
    pub has_value: bool,
    pub refs: Vec<LocalRef>,
}

impl Local {
    pub fn is_read(&self) -> bool {
        self.refs.iter().any(|r| !r.write)
    }

    pub fn is_visible_at(&self, offset: u32) -> bool {
        self.visible_from <= offset && offset <= self.scope_end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalRefKind {
    Read,
    Write,
    FunctionDecl,
}

#[derive(Clone, Debug)]
pub struct GlobalRef {
    pub name: SmolStr,
    pub span: Span,
    pub kind: GlobalRefKind,
    pub func: FuncId,
}

impl GlobalRef {
    pub fn is_definition(&self) -> bool {
        self.kind != GlobalRefKind::Read
    }
}

#[derive(Clone, Debug)]
pub struct Function {
    pub span: Span,
    pub parent: Option<FuncId>,
}

#[derive(Clone, Debug)]
pub struct Label {
    pub name: SmolStr,
    pub span: Span,
    pub used: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    Local(LocalId),
    Global(u32),
}

#[derive(Debug, Default)]
pub struct Resolution {
    pub locals: Vec<Local>,
    pub globals: Vec<GlobalRef>,
    pub functions: Vec<Function>,
    pub labels: Vec<Label>,
    pub undefined_gotos: Vec<Name>,
    by_offset: FxHashMap<u32, Resolved>,
}

impl Resolution {
    pub fn resolve_at(&self, name_start: u32) -> Option<Resolved> {
        self.by_offset.get(&name_start).copied()
    }

    pub fn local(&self, id: LocalId) -> &Local {
        &self.locals[id as usize]
    }

    pub fn locals_visible_at(&self, offset: u32) -> impl Iterator<Item = (LocalId, &Local)> {
        self.locals.iter().enumerate().filter(move |(_, l)| l.is_visible_at(offset)).map(|(i, l)| (i as LocalId, l))
    }

    /// The innermost visible local with this name at `offset`.
    pub fn lookup_local_at(&self, name: &str, offset: u32) -> Option<LocalId> {
        self.locals_visible_at(offset)
            .filter(|(_, l)| l.name == name)
            .max_by_key(|(_, l)| l.visible_from)
            .map(|(id, _)| id)
    }

    /// Finds the identifier occurrence (declaration or reference) covering `offset`.
    pub fn resolved_at_offset(&self, offset: u32) -> Option<(Resolved, Span)> {
        for (id, local) in self.locals.iter().enumerate() {
            if local.decl.contains_inclusive(offset) {
                return Some((Resolved::Local(id as LocalId), local.decl));
            }
            if let Some(r) = local.refs.iter().find(|r| r.span.contains_inclusive(offset)) {
                return Some((Resolved::Local(id as LocalId), r.span));
            }
        }
        self.globals
            .iter()
            .enumerate()
            .find(|(_, g)| g.span.contains_inclusive(offset))
            .map(|(i, g)| (Resolved::Global(i as u32), g.span))
    }
}

pub fn resolve(chunk: &Chunk) -> Resolution {
    let mut resolver = Resolver {
        out: Resolution::default(),
        scopes: Vec::new(),
        func_stack: vec![MAIN_CHUNK],
        label_scopes: Vec::new(),
    };
    resolver.out.functions.push(Function { span: chunk.block.span, parent: None });
    resolver.block(&chunk.block, u32::MAX);
    resolver.out
}

struct Scope {
    names: Vec<(SmolStr, LocalId)>,
}

struct LabelScope {
    func: FuncId,
    labels: Vec<(SmolStr, usize)>,
}

struct Resolver {
    out: Resolution,
    scopes: Vec<Scope>,
    func_stack: Vec<FuncId>,
    label_scopes: Vec<LabelScope>,
}

impl Resolver {
    fn func(&self) -> FuncId {
        *self.func_stack.last().unwrap_or(&MAIN_CHUNK)
    }

    fn lookup(&self, name: &str) -> Option<LocalId> {
        self.scopes.iter().rev().find_map(|s| s.names.iter().rev().find(|(n, _)| n == name).map(|(_, id)| *id))
    }

    #[allow(clippy::too_many_arguments)]
    fn declare(
        &mut self,
        name: &Name,
        kind: LocalKind,
        attrib: Option<Attrib>,
        visible_from: u32,
        scope_end: u32,
        has_value: bool,
    ) -> LocalId {
        let id = self.out.locals.len() as LocalId;
        let scope = self.scopes.last().expect("scope stack is never empty while declaring");
        let redefines = scope.names.iter().rev().find(|(n, _)| *n == name.text).map(|(_, id)| *id);
        let shadows = if redefines.is_some() { None } else { self.lookup(&name.text) };
        self.out.locals.push(Local {
            name: name.text.clone(),
            decl: name.span,
            kind,
            attrib,
            func: self.func(),
            visible_from,
            scope_end,
            shadows,
            redefines,
            has_value,
            refs: Vec::new(),
        });
        if !name.is_missing() {
            self.out.by_offset.insert(name.span.start, Resolved::Local(id));
            self.scopes.last_mut().unwrap().names.push((name.text.clone(), id));
        }
        id
    }

    fn reference(&mut self, name: &Name, write: bool) {
        if name.is_missing() {
            return;
        }
        let func = self.func();
        match self.lookup(&name.text) {
            Some(id) => {
                self.out.locals[id as usize].refs.push(LocalRef { span: name.span, write, func });
                self.out.by_offset.insert(name.span.start, Resolved::Local(id));
            }
            None => {
                let kind = if write { GlobalRefKind::Write } else { GlobalRefKind::Read };
                self.global(name, kind);
            }
        }
    }

    fn global(&mut self, name: &Name, kind: GlobalRefKind) {
        let index = self.out.globals.len() as u32;
        self.out.globals.push(GlobalRef { name: name.text.clone(), span: name.span, kind, func: self.func() });
        self.out.by_offset.insert(name.span.start, Resolved::Global(index));
    }

    fn block(&mut self, block: &Block, scope_end: u32) {
        self.scopes.push(Scope { names: Vec::new() });
        self.stmts(block, scope_end);
        self.scopes.pop();
    }

    /// The statements of `block`, declaring their locals in the innermost scope.
    fn stmts(&mut self, block: &Block, scope_end: u32) {
        self.push_labels(block);
        for stmt in &block.stmts {
            self.stmt(stmt, scope_end);
        }
        self.label_scopes.pop();
    }

    fn push_labels(&mut self, block: &Block) {
        let mut labels = Vec::new();
        for stmt in &block.stmts {
            if let StmtKind::Label(name) = &stmt.kind {
                labels.push((name.text.clone(), self.out.labels.len()));
                self.out.labels.push(Label { name: name.text.clone(), span: name.span, used: false });
            }
        }
        self.label_scopes.push(LabelScope { func: self.func(), labels });
    }

    fn goto(&mut self, name: &Name) {
        let func = self.func();
        let target = self
            .label_scopes
            .iter()
            .rev()
            .take_while(|s| s.func == func)
            .find_map(|s| s.labels.iter().find(|(n, _)| *n == name.text).map(|(_, i)| *i));
        match target {
            Some(index) => self.out.labels[index].used = true,
            None if !name.is_missing() => self.out.undefined_gotos.push(name.clone()),
            None => {}
        }
    }

    fn stmt(&mut self, stmt: &Stmt, scope_end: u32) {
        match &stmt.kind {
            StmtKind::Local { names, exprs, .. } => {
                for expr in exprs {
                    self.expr(expr);
                }
                for (i, name) in names.iter().enumerate() {
                    let has_value = i < exprs.len() || exprs.last().is_some_and(Expr::is_multi_value);
                    let attrib = name.attrib.map(|(a, _)| a);
                    self.declare(&name.name, LocalKind::Local, attrib, stmt.span.end, scope_end, has_value);
                }
            }
            StmtKind::LocalFunction { name, func } => {
                self.declare(name, LocalKind::LocalFunction, None, name.span.start, scope_end, true);
                self.function(func, false);
            }
            StmtKind::Function { name, func } => {
                if name.path.is_empty() && name.method.is_none() {
                    match self.lookup(&name.base.text) {
                        Some(_) => self.reference(&name.base, true),
                        None => self.global(&name.base, GlobalRefKind::FunctionDecl),
                    }
                } else {
                    self.reference(&name.base, false);
                }
                self.function(func, name.method.is_some());
            }
            StmtKind::Assign { targets, exprs } => {
                for expr in exprs {
                    self.expr(expr);
                }
                for target in targets {
                    self.assign_target(target);
                }
            }
            StmtKind::CompoundAssign { target, expr, .. } => {
                self.expr(expr);
                self.expr(target);
                self.assign_target(target);
            }
            StmtKind::Expr(expr) => self.expr(expr),
            StmtKind::Do(body) | StmtKind::Defer(body) => self.block(body, stmt.span.end),
            StmtKind::While { cond, body } => {
                self.expr(cond);
                self.block(body, stmt.span.end);
            }
            StmtKind::Repeat { body, cond } => {
                self.scopes.push(Scope { names: Vec::new() });
                self.push_labels(body);
                for inner in &body.stmts {
                    self.stmt(inner, stmt.span.end);
                }
                self.expr(cond);
                self.label_scopes.pop();
                self.scopes.pop();
            }
            StmtKind::If { branches, else_block } => {
                for (i, branch) in branches.iter().enumerate() {
                    self.expr(&branch.cond);
                    let end = branches
                        .get(i + 1)
                        .map(|next| next.keyword_span.start)
                        .or(else_block.as_ref().map(|b| b.span.start))
                        .unwrap_or(stmt.span.end);
                    self.block(&branch.block, end.max(branch.block.span.end));
                }
                if let Some(block) = else_block {
                    self.block(block, stmt.span.end);
                }
            }
            StmtKind::NumericFor { var, start, limit, step, body } => {
                self.expr(start);
                self.expr(limit);
                if let Some(step) = step {
                    self.expr(step);
                }
                self.scopes.push(Scope { names: Vec::new() });
                // Visible from the end of the header, as parameters are from the `)`, so a comment
                // above the first statement of the body, like a `---@cast`, sees the variable.
                let header_end = step.as_ref().unwrap_or(limit).span.end;
                self.declare(var, LocalKind::LoopVar, None, header_end, stmt.span.end, true);
                self.block(body, stmt.span.end);
                self.scopes.pop();
            }
            StmtKind::GenericFor { names, exprs, body } => {
                for expr in exprs {
                    self.expr(expr);
                }
                self.scopes.push(Scope { names: Vec::new() });
                let header_end = exprs.last().map_or(body.span.start, |expr| expr.span.end);
                for name in names {
                    self.declare(name, LocalKind::LoopVar, None, header_end, stmt.span.end, true);
                }
                self.block(body, stmt.span.end);
                self.scopes.pop();
            }
            StmtKind::Return(exprs) => {
                for expr in exprs {
                    self.expr(expr);
                }
            }
            StmtKind::Goto(name) => self.goto(name),
            StmtKind::Break | StmtKind::Label(_) | StmtKind::Error => {}
        }
    }

    fn assign_target(&mut self, target: &Expr) {
        match &target.kind {
            ExprKind::Name(name) => self.reference(name, true),
            _ => self.expr(target),
        }
    }

    fn function(&mut self, func: &FuncBody, implicit_self: bool) {
        let id = self.out.functions.len() as FuncId;
        self.out.functions.push(Function { span: func.span, parent: Some(self.func()) });
        self.func_stack.push(id);
        self.scopes.push(Scope { names: Vec::new() });
        let body_start = func.params_span.end;
        if implicit_self {
            let name = Name { text: SmolStr::new_static("self"), span: Span::empty(func.params_span.start) };
            self.declare(&name, LocalKind::ImplicitSelf, None, body_start, func.span.end, true);
            self.out.by_offset.remove(&name.span.start);
        }
        for param in &func.params {
            self.declare(param, LocalKind::Param, None, body_start, func.span.end, true);
        }
        // The parameters and the locals of the body share one block, as in Lua, so a `local` of the
        // body that takes the name of a parameter redefines it.
        self.stmts(&func.body, func.span.end);
        self.scopes.pop();
        self.func_stack.pop();
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Name(name) => self.reference(name, false),
            ExprKind::Function(func) => self.function(func, false),
            ExprKind::Index { base, index, .. } => {
                self.expr(base);
                self.expr(index);
            }
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Call { callee, args, .. } => {
                self.expr(callee);
                for arg in args {
                    self.expr(arg);
                }
            }
            ExprKind::MethodCall { base, args, .. } => {
                self.expr(base);
                for arg in args {
                    self.expr(arg);
                }
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
            ExprKind::Nil
            | ExprKind::True
            | ExprKind::False
            | ExprKind::Vararg
            | ExprKind::Number(_)
            | ExprKind::String(_)
            | ExprKind::JenkinsHash(_)
            | ExprKind::Error => {}
        }
    }
}
