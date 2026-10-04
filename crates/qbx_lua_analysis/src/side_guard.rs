use qbx_fivem_data::Side;
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{SmolStr, Span};
use rustc_hash::FxHashMap;

/// Parts of a file that only run on one side, whatever the manifest says about the file:
/// the branches of `if IsDuplicityVersion() then ... else ... end`, of `lib.context == 'server'`,
/// the code after `if not IsDuplicityVersion() then return end`, and the right side of
/// `IsDuplicityVersion() and ...`.
#[derive(Debug, Default)]
pub struct SideRegions {
    regions: Vec<(Span, Side)>,
}

impl SideRegions {
    pub fn of(source: &str, chunk: &Chunk) -> Self {
        if !source.contains("IsDuplicityVersion") && !source.contains("lib.context") {
            return Self::default();
        }
        Self::of_chunk(chunk)
    }

    pub fn of_chunk(chunk: &Chunk) -> Self {
        let mut flags = Flags::default();
        flags.visit_block(&chunk.block);
        let mut finder = Finder { flags: flags.names, regions: Vec::new() };
        finder.visit_block(&chunk.block);
        Self { regions: finder.regions }
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// The side the code at `offset` is restricted to, taken from the innermost guard around it.
    pub fn side_at(&self, offset: u32) -> Option<Side> {
        self.regions
            .iter()
            .filter(|(span, _)| span.start <= offset && offset < span.end)
            .min_by_key(|(span, _)| span.end - span.start)
            .map(|(_, side)| *side)
    }

    pub fn effective(&self, offset: u32, file_side: Option<Side>) -> Option<Side> {
        self.side_at(offset).or(file_side)
    }
}

pub(crate) fn other(side: Side) -> Side {
    match side {
        Side::Client => Side::Server,
        Side::Server => Side::Client,
        Side::Shared => Side::Shared,
    }
}

fn is_duplicity_call(expr: &Expr) -> bool {
    match &expr.unparen().kind {
        ExprKind::Call { callee, args, .. } => {
            args.is_empty() && matches!(&callee.kind, ExprKind::Name(name) if name.text == "IsDuplicityVersion")
        }
        _ => false,
    }
}

fn context_literal(expr: &Expr) -> Option<Side> {
    match expr.unparen().as_string()?.as_str() {
        "server" => Some(Side::Server),
        "client" => Some(Side::Client),
        _ => None,
    }
}

fn is_context_read(expr: &Expr) -> bool {
    matches!(expr.unparen().dotted_path().as_deref(), Some("lib.context"))
}

/// The side on which `cond` is true, and whether it is false everywhere on the other side.
fn side_when_true(cond: &Expr, flags: &FxHashMap<SmolStr, Side>) -> Option<(Side, bool)> {
    let cond = cond.unparen();
    if is_duplicity_call(cond) {
        return Some((Side::Server, true));
    }
    match &cond.kind {
        ExprKind::Name(name) => flags.get(&name.text).map(|side| (*side, true)),
        ExprKind::Unary { op: UnOp::Not, expr } => {
            let (side, exact) = side_when_true(expr, flags)?;
            exact.then_some((other(side), true))
        }
        ExprKind::Binary { op: op @ (BinOp::Eq | BinOp::Ne), lhs, rhs, .. } => {
            let side = match (is_context_read(lhs), is_context_read(rhs)) {
                (true, _) => context_literal(rhs)?,
                (_, true) => context_literal(lhs)?,
                _ => return None,
            };
            Some((if *op == BinOp::Eq { side } else { other(side) }, true))
        }
        ExprKind::Binary { op: BinOp::And, lhs, rhs, .. } => {
            let (side, _) = side_when_true(lhs, flags).or_else(|| side_when_true(rhs, flags))?;
            Some((side, false))
        }
        _ => None,
    }
}

/// Names such as `local isServer = IsDuplicityVersion()`.
#[derive(Default)]
struct Flags {
    names: FxHashMap<SmolStr, Side>,
}

impl Flags {
    fn record(&mut self, name: &SmolStr, value: &Expr) {
        if let Some((side, true)) = side_when_true(value, &self.names) {
            self.names.insert(name.clone(), side);
        }
    }
}

impl<'ast> Visitor<'ast> for Flags {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match &stmt.kind {
            StmtKind::Local { names, exprs, .. } if names.len() == 1 && exprs.len() == 1 => {
                self.record(&names[0].name.text, &exprs[0]);
            }
            StmtKind::Assign { targets, exprs } if targets.len() == 1 && exprs.len() == 1 => {
                if let ExprKind::Name(name) = &targets[0].kind {
                    self.record(&name.text, &exprs[0]);
                }
            }
            _ => {}
        }
        visit::walk_stmt(self, stmt);
    }
}

struct Finder {
    flags: FxHashMap<SmolStr, Side>,
    regions: Vec<(Span, Side)>,
}

impl Finder {
    /// `if <not this side> then return end`: the side the rest of the block runs on.
    fn guard(&self, stmt: &Stmt) -> Option<Side> {
        let StmtKind::If { branches, else_block: None } = &stmt.kind else { return None };
        let [only] = branches.as_slice() else { return None };
        let leaves = matches!(only.block.stmts.last(), Some(Stmt { kind: StmtKind::Return(_), .. }));
        match side_when_true(&only.cond, &self.flags) {
            Some((side, true)) if leaves => Some(other(side)),
            _ => None,
        }
    }
}

impl<'ast> Visitor<'ast> for Finder {
    fn visit_block(&mut self, block: &'ast Block) {
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
            if let Some(side) = self.guard(stmt) {
                self.regions.push((Span { start: stmt.span.end, end: block.span.end.max(stmt.span.end) }, side));
            }
        }
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let StmtKind::If { branches, else_block } = &stmt.kind {
            let mut excluded = Vec::new();
            let else_start = else_block.as_ref().map(|block| block.span.start.min(stmt.span.end));
            for (index, branch) in branches.iter().enumerate() {
                let Some((side, exact)) = side_when_true(&branch.cond, &self.flags) else { continue };
                // Measured between the keywords so half-typed code inside the branch still counts.
                let next = branches.get(index + 1).map(|b| b.keyword_span.start);
                let end = next.or(else_start).unwrap_or(stmt.span.end);
                self.regions.push((Span { start: branch.cond.span.end, end: end.max(branch.cond.span.end) }, side));
                if exact {
                    excluded.push(side);
                }
            }
            if let (Some(start), [side]) = (else_start, excluded.as_slice()) {
                self.regions.push((Span { start, end: stmt.span.end }, other(*side)));
            }
        }
        visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // The right side of `isServer and os.time()` only runs where the left side is true.
        if let ExprKind::Binary { op: BinOp::And, lhs, rhs, .. } = &expr.kind {
            if let Some((side, _)) = side_when_true(lhs, &self.flags) {
                self.regions.push((rhs.span, side));
            }
        }
        visit::walk_expr(self, expr);
    }
}
