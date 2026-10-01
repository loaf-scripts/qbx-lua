use std::sync::OnceLock;

use qbx_fivem_data::{Side, STUBS};
use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::{parse, Comment, CommentKind, SmolStr};
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Debug)]
pub struct BuiltinGlobal {
    pub side: Side,
    pub fields: FxHashSet<SmolStr>,
    pub deprecated: bool,
    pub deprecated_fields: FxHashSet<SmolStr>,
}

#[derive(Debug, Default)]
pub struct Builtins {
    globals: FxHashMap<SmolStr, BuiltinGlobal>,
}

/// Library tables whose field set is fully known, so reading anything else is a bug.
const CLOSED_TABLES: &[&str] = &["string", "table", "os", "io", "coroutine", "utf8", "json", "Citizen"];

const EXTRA_GLOBALS: &[&str] = &["glm", "_ENV"];

impl Builtins {
    pub fn get(&self, name: &str) -> Option<&BuiltinGlobal> {
        self.globals.get(name)
    }

    pub fn is_defined(&self, name: &str, side: Side) -> bool {
        self.get(name).is_some_and(|g| g.side.is_available_on(side))
    }

    pub fn closed_table(&self, name: &str) -> Option<&BuiltinGlobal> {
        CLOSED_TABLES.contains(&name).then(|| self.get(name)).flatten()
    }

    pub fn names(&self) -> impl Iterator<Item = (&SmolStr, &BuiltinGlobal)> {
        self.globals.iter()
    }

    fn entry(&mut self, name: &SmolStr, side: Side) -> &mut BuiltinGlobal {
        self.globals.entry(name.clone()).or_insert_with(|| BuiltinGlobal {
            side,
            fields: FxHashSet::default(),
            deprecated: false,
            deprecated_fields: FxHashSet::default(),
        })
    }

    fn load_stub(&mut self, source: &str, side: Side) {
        let chunk = parse(source);
        for stmt in &chunk.block.stmts {
            let deprecated = leading_doc_lines(source, &chunk.comments, stmt.span.start)
                .iter()
                .any(|line| line.trim_start().starts_with("@deprecated"));
            match &stmt.kind {
                StmtKind::Function { name, .. } => {
                    let member = name.path.first().or(name.method.as_ref());
                    self.define(&name.base.text, member.map(|m| &m.text), side, deprecated);
                }
                StmtKind::Assign { targets, .. } => {
                    for target in targets {
                        match &target.kind {
                            ExprKind::Name(name) => self.define(&name.text, None, side, deprecated),
                            ExprKind::Field { base, name, .. } => {
                                if let ExprKind::Name(base) = &base.kind {
                                    self.define(&base.text, Some(&name.text), side, deprecated);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn define(&mut self, name: &SmolStr, field: Option<&SmolStr>, side: Side, deprecated: bool) {
        let global = self.entry(name, side);
        match field {
            Some(field) => {
                global.fields.insert(field.clone());
                if deprecated {
                    global.deprecated_fields.insert(field.clone());
                }
            }
            None => global.deprecated |= deprecated,
        }
    }
}

pub fn builtins() -> &'static Builtins {
    static BUILTINS: OnceLock<Builtins> = OnceLock::new();
    BUILTINS.get_or_init(|| {
        let mut builtins = Builtins::default();
        for stub in STUBS {
            builtins.load_stub(stub.source, stub.side);
        }
        for name in EXTRA_GLOBALS {
            builtins.entry(&SmolStr::new_static(name), Side::Shared);
        }
        builtins
    })
}

/// The `---` doc lines directly above `offset`, top to bottom, with the `---` prefix removed. Like
/// LuaLS, other comments on lines of their own are passed over, so a `-- qbx-lint:` directive
/// between the docs and the code keeps them; a blank line or code ends the block.
pub fn leading_doc_lines<'a>(source: &'a str, comments: &[Comment], offset: u32) -> Vec<&'a str> {
    let mut lines = Vec::new();
    let mut cursor = offset;
    let end = comments.partition_point(|c| c.span.end <= offset);
    for comment in comments[..end].iter().rev() {
        let gap = &source[comment.span.end as usize..cursor as usize];
        if gap.bytes().filter(|b| *b == b'\n').count() > 1 || !gap.trim().is_empty() {
            break;
        }
        if !source[..comment.span.start as usize].rsplit('\n').next().unwrap_or("").trim().is_empty() {
            break;
        }
        if let Some(doc) = comment.span.text(source).strip_prefix("---") {
            lines.push(doc);
        }
        cursor = comment.span.start;
    }
    lines.reverse();
    lines
}

/// Runs of adjacent `---` line comments, each one annotation block. Like LuaLS, other comments
/// inside a run are passed over; a blank line or code ends it.
pub fn doc_blocks<'c>(source: &str, comments: &'c [Comment]) -> Vec<Vec<&'c Comment>> {
    let mut blocks: Vec<Vec<&Comment>> = Vec::new();
    let mut block: Vec<&Comment> = Vec::new();
    let mut previous_end: Option<u32> = None;
    for comment in comments {
        let adjacent = previous_end.is_some_and(|end| {
            let gap = &source[end as usize..comment.span.start as usize];
            gap.bytes().filter(|b| *b == b'\n').count() <= 1 && gap.trim().is_empty()
        });
        if !adjacent && !block.is_empty() {
            blocks.push(std::mem::take(&mut block));
        }
        if comment.kind == CommentKind::Line && comment.span.text(source).starts_with("---") {
            block.push(comment);
        }
        previous_end = Some(comment.span.end);
    }
    if !block.is_empty() {
        blocks.push(block);
    }
    blocks
}

/// Whether a `---@meta` line above the first statement marks the file as a definition file.
pub fn is_meta_file(source: &str, chunk: &Chunk) -> bool {
    let first_stmt = chunk.block.stmts.first().map_or(u32::MAX, |stmt| stmt.span.start);
    chunk
        .comments
        .iter()
        .take_while(|comment| comment.span.start < first_stmt)
        .any(|comment| is_meta_comment(comment.span.text(source)))
}

fn is_meta_comment(text: &str) -> bool {
    let doc = text.strip_prefix("---").unwrap_or_default();
    doc.trim_start().strip_prefix("@meta").is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubs_define_the_runtime() {
        let builtins = builtins();
        for name in [
            "print",
            "pairs",
            "string",
            "Citizen",
            "Wait",
            "CreateThread",
            "exports",
            "json",
            "vector3",
            "vec3",
            "source",
        ] {
            assert!(builtins.is_defined(name, Side::Shared), "{name} should be a shared builtin");
        }
        assert!(builtins.is_defined("TriggerServerEvent", Side::Client));
        assert!(!builtins.is_defined("TriggerServerEvent", Side::Server));
        assert!(builtins.is_defined("TriggerClientEvent", Side::Server));
        assert!(!builtins.is_defined("TriggerClientEvent", Side::Client));
        assert!(builtins.closed_table("string").unwrap().fields.contains("format"));
        assert!(builtins.closed_table("Citizen").unwrap().fields.contains("Wait"));
        assert!(builtins.closed_table("math").is_none());
    }

    #[test]
    fn doc_lines_stop_at_blank_lines_and_code() {
        let source =
            "--- unrelated\n\n--- first\n---@deprecated\nfunction f() end\nlocal x = 1 --- trailing\nfunction g() end";
        let chunk = parse(source);
        let f = chunk.block.stmts[0].span.start;
        assert_eq!(leading_doc_lines(source, &chunk.comments, f), [" first", "@deprecated"]);
        let g = chunk.block.stmts[2].span.start;
        assert!(leading_doc_lines(source, &chunk.comments, g).is_empty());
    }

    #[test]
    fn doc_lines_pass_over_other_comments_on_their_own_lines() {
        let source = "---@param a number\n-- qbx-lint: disable-next-line lowercase-global\nfunction lower(a) end\n\
            ---@param b number\n-- one\n-- two\nlocal function plain(b) end\n\
            ---@param c number\n--[[ block\ncomment ]]\nlocal function block(c) end\n\
            --- first\n-- note\n---@param d number\nlocal function middle(d) end\n\
            ---@param e number\n-- one\n\nlocal function blank(e) end\n\
            ---@param f number\nlocal x = 1 -- trailing\nlocal function trailing(f) end";
        let chunk = parse(source);
        let docs = |index: usize| leading_doc_lines(source, &chunk.comments, chunk.block.stmts[index].span.start);
        assert_eq!(docs(0), ["@param a number"], "a qbx-lint directive");
        assert_eq!(docs(1), ["@param b number"], "plain comments");
        assert_eq!(docs(2), ["@param c number"], "a block comment");
        assert_eq!(docs(3), [" first", "@param d number"], "a plain comment inside the docs");
        assert!(docs(4).is_empty(), "a blank line after a plain comment");
        assert!(docs(6).is_empty(), "a comment after code");
    }
}
