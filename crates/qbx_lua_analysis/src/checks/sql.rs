//! The `sql/` rules: the names in the queries a resource sends its database, such as columns and
//! tables, that the database, or the other one of MariaDB and MySQL, reserves and that have no
//! backticks.
//! `SELECT key FROM t` fails on MariaDB and MySQL, which reserve `key`, while ``SELECT `key` FROM t``
//! runs.
//!
//! A query is a string written out as the first argument of a call of a database library, as
//! `MySQL.query.await('SELECT ...')` or `exports.oxmysql:execute('...')`, each query of a
//! transaction, a string on the line below `---@language sql`, and, where the language server tells,
//! the first argument of a function whose first parameter has the `sql` type. A string built at
//! runtime is not read, and neither is a query the parser does not understand, as a statement only
//! one database has: what such a query names stays unknown.

use std::sync::OnceLock;

use qbx_lua_syntax::ast::*;
use qbx_lua_syntax::visit::{self, Visitor};
use qbx_lua_syntax::{LineIndex, Span};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::Value;
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

use super::{FileInput, Sink};
use crate::config::Database;
use crate::diagnostic::{Fix, TextEdit};
use crate::rules;

/// The tables and resources of the database libraries, whose functions take a query first.
const SQL_ROOTS: &[&str] = &["MySQL", "exports.oxmysql", "exports.ghmattimysql", "exports.mysql-async"];

/// Whether `path`, the function a call calls, or the table a method is called on, belongs to a
/// database library.
pub(super) fn is_database_call(path: &str) -> bool {
    SQL_ROOTS.iter().any(|root| path == *root || path.strip_prefix(root).is_some_and(|rest| rest.starts_with('.')))
}

/// The reserved words of MariaDB and MySQL, as their documentation lists them.
static RESERVED_WORDS: &str = include_str!("../../data/sql_reserved_words.tsv");

/// Which of the databases reserve a word.
struct Reserved {
    mariadb: bool,
    mysql: bool,
    /// The MariaDB releases that reserve it, as `10.6+`, where not all of them do.
    note: &'static str,
}

/// Which of MariaDB and MySQL reserve `word`, whatever its case, when one of them does.
fn reserved(word: &str) -> Option<&'static Reserved> {
    static TABLE: OnceLock<FxHashMap<&'static str, Reserved>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let lines = RESERVED_WORDS.lines().filter(|line| !line.is_empty() && !line.starts_with('#'));
        lines
            .filter_map(|line| {
                let mut columns = line.split('\t');
                let word = columns.next()?;
                let (mariadb, mysql) = (columns.next() == Some("R"), columns.next() == Some("R"));
                Some((word, Reserved { mariadb, mysql, note: columns.next().unwrap_or("") }))
            })
            .collect()
    });
    table.get(word.to_ascii_uppercase().as_str())
}

/// Words a query writes as values, as the `DEFAULT` of `VALUES (DEFAULT, ?)`, which the parser reads
/// as names.
const VALUE_WORDS: &[&str] = &[
    "DEFAULT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "CURRENT_ROLE",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "UTC_DATE",
    "UTC_TIME",
    "UTC_TIMESTAMP",
    "MAXVALUE",
];

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    let codes = [rules::SQL_UNQUOTED_RESERVED_WORD, rules::SQL_RESERVED_IN_OTHER_DATABASE];
    if codes.iter().all(|code| sink.enabled(code).is_none()) {
        return;
    }
    let lines = LineIndex::new(input.source);
    let mut finder = Queries { input, marked: marked_lines(input, &lines), lines: &lines, found: Vec::new() };
    finder.visit_block(&input.chunk.block);
    let mut seen = FxHashSet::default();
    for query in finder.found {
        if seen.insert(query.start) {
            check_query(input, sink, query);
        }
    }
}

/// The lines below a `---@language sql` comment on a line of its own, whose strings are queries, as
/// the editor highlights them.
fn marked_lines(input: &FileInput, lines: &LineIndex) -> FxHashSet<u32> {
    let marks = input.chunk.comments.iter().filter(|comment| {
        let line = lines.line_of(comment.span.start);
        let before = Span::new(lines.line_start(line), comment.span.start).text(input.source);
        let Some(rest) = comment.span.text(input.source).strip_prefix("---@language") else { return false };
        let language = rest.trim_start_matches([' ', '\t']);
        before.chars().all(|c| c == ' ' || c == '\t')
            && language.len() < rest.len()
            && language
                .strip_prefix("sql")
                .is_some_and(|after| !after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'))
    });
    marks.map(|comment| lines.line_of(comment.span.start) + 1).collect()
}

/// Finds the strings of a file that are queries.
struct Queries<'a, 'i> {
    input: &'a FileInput<'i>,
    lines: &'a LineIndex,
    marked: FxHashSet<u32>,
    found: Vec<Span>,
}

impl Queries<'_, '_> {
    /// Notes the queries that the first argument of `call` writes out when the call sends it to the
    /// database: a string, or for a transaction, the strings of a table and the `query` of the
    /// tables in it.
    fn call(&mut self, call: &Expr, path: Option<String>, args: &[Expr]) {
        let Some(arg) = args.first().map(Expr::unparen) else { return };
        if !matches!(arg.kind, ExprKind::String(_) | ExprKind::Table(_)) {
            return;
        }
        let sends =
            path.is_some_and(|path| is_database_call(&path)) || self.input.takes_query.is_some_and(|takes| takes(call));
        if !sends {
            return;
        }
        match &arg.kind {
            ExprKind::String(_) => self.found.push(arg.span),
            ExprKind::Table(fields) => {
                for field in fields {
                    match field {
                        TableField::Positional(value) => match &value.unparen().kind {
                            ExprKind::String(_) => self.string(value),
                            ExprKind::Table(inner) => self.transaction_entry(inner),
                            _ => {}
                        },
                        TableField::Named { name, value } if name.text == "query" => self.string(value),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    /// The query of an entry of a transaction, as `{ query = '...', values = {...} }` or
    /// `{ '...', {...} }`.
    fn transaction_entry(&mut self, fields: &[TableField]) {
        for field in fields {
            match field {
                TableField::Named { name, value } if name.text == "query" => return self.string(value),
                TableField::Positional(value) => return self.string(value),
                _ => {}
            }
        }
    }

    fn string(&mut self, value: &Expr) {
        if let ExprKind::String(_) = value.unparen().kind {
            self.found.push(value.unparen().span);
        }
    }
}

impl<'q> Visitor<'q> for Queries<'_, '_> {
    fn visit_expr(&mut self, expr: &'q Expr) {
        match &expr.kind {
            ExprKind::Call { callee, args, .. } => self.call(expr, callee.dotted_path(), args),
            ExprKind::MethodCall { base, args, .. } => self.call(expr, base.dotted_path(), args),
            ExprKind::String(_) if self.marked.contains(&self.lines.line_of(expr.span.start)) => {
                self.found.push(expr.span);
            }
            _ => {}
        }
        visit::walk_expr(self, expr);
    }
}

/// A name that a query writes without backticks, where the parser puts it: a line and a column of
/// the query, counted from 1, and its end.
struct Name {
    text: String,
    start: (usize, usize),
    end: (usize, usize),
}

fn check_query(input: &FileInput, sink: &mut Sink, string: Span) {
    let Some(content) = Content::of(input.source, string) else { return };
    let Ok(statements) = Parser::parse_sql(&MySqlDialect {}, &content.text) else { return };
    let mut names = Vec::new();
    for statement in &statements {
        if let Ok(value) = serde_json::to_value(statement) {
            collect_names(&value, None, &mut names);
        }
    }
    for name in names {
        let (Some(start), Some(end)) = (content.offset(name.start), content.offset(name.end)) else { continue };
        report(input, sink, &name.text, Span::new(start, end));
    }
}

/// Collects the names that `value`, a statement as serde writes it, holds under `key`: the
/// identifiers without quotes, leaving out the names of functions, as `COUNT` of `COUNT(*)`, the
/// options of a table, as `ENGINE` of `ENGINE=InnoDB`, variables, as `@name`, and the words after
/// the first `.` of a qualified name.
fn collect_names(value: &Value, key: Option<&str>, out: &mut Vec<Name>) {
    match value {
        Value::Object(map) => {
            if let (Some(text), Some(quote)) = (map.get("value").and_then(Value::as_str), map.get("quote_style")) {
                let at = |side: &str| {
                    let location = map.get("span")?.get(side)?;
                    Some((location.get("line")?.as_u64()? as usize, location.get("column")?.as_u64()? as usize))
                };
                if let (true, Some(start), Some(end)) = (quote.is_null(), at("start"), at("end")) {
                    if start.0 > 0
                        && !text.starts_with('@')
                        && !VALUE_WORDS.contains(&text.to_ascii_uppercase().as_str())
                    {
                        out.push(Name { text: text.to_string(), start, end });
                    }
                }
                return;
            }
            for (inner_key, inner) in map {
                if inner_key == "table_options" || (inner_key == "name" && key == Some("Function")) {
                    continue;
                }
                collect_names(inner, Some(inner_key), out);
            }
        }
        // The databases read the words after the first `.` of a qualified name, as `from` of
        // `n.from` or `key` of `db.key`, as names, also those they reserve.
        Value::Array(items) if key == Some("CompoundIdentifier") || items.iter().all(is_name_part) => {
            if let Some(first) = items.first() {
                collect_names(first, key, out);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_names(item, key, out)),
        _ => {}
    }
}

/// Whether `value` is a part of the name of a table or another object, as `{"Identifier": ...}`.
fn is_name_part(value: &Value) -> bool {
    value.as_object().is_some_and(|map| map.len() == 1 && map.contains_key("Identifier"))
}

fn report(input: &FileInput, sink: &mut Sink, word: &str, span: Span) {
    let Some(reserved) = reserved(word) else { return };
    let database = input.config.database;
    let (here, other) = match database {
        Database::MariaDb => (reserved.mariadb, reserved.mysql),
        Database::MySql => (reserved.mysql, reserved.mariadb),
    };
    let named = |database: Database| match database {
        Database::MariaDb if reserved.note.is_empty() => "MariaDB".to_string(),
        Database::MariaDb => format!("MariaDB {}", reserved.note),
        Database::MySql => "MySQL".to_string(),
    };
    let other_database = match database {
        Database::MariaDb => Database::MySql,
        Database::MySql => Database::MariaDb,
    };
    let (code, message) = match (here, other) {
        (true, _) => (
            rules::SQL_UNQUOTED_RESERVED_WORD,
            format!("'{word}' is a reserved word in {}, so the query fails; quote it as `{word}`", named(database)),
        ),
        (false, true) => (
            rules::SQL_RESERVED_IN_OTHER_DATABASE,
            format!(
                "'{word}' is a reserved word in {}; quote it as `{word}` for the query to run there too",
                named(other_database)
            ),
        ),
        (false, false) => return,
    };
    let edits = vec![
        TextEdit { span: Span::new(span.start, span.start), new_text: "`".to_string() },
        TextEdit { span: Span::new(span.end, span.end), new_text: "`".to_string() },
    ];
    let fix = Fix { title: format!("Quote '{word}' with backticks"), edits, suggestion: false };
    sink.report_with(code, span, message, None, Some(fix));
}

/// The text of a string literal, with where each of its bytes is written in the source.
struct Content {
    text: String,
    /// The offset in the source of each byte of `text`, and of its end.
    offsets: Vec<u32>,
}

impl Content {
    /// The content of the string literal at `span`: the text between its quotes with its escapes
    /// read, or between the brackets of a long string without the line break that follows the
    /// opening one.
    fn of(source: &str, span: Span) -> Option<Self> {
        let raw = source.get(span.start as usize..span.end as usize)?;
        let mut content = Content { text: String::new(), offsets: Vec::new() };
        if let Some(bracket) = raw.strip_prefix('[') {
            let level = bracket.find('[')?;
            let open = level + 2;
            let close = raw.len().checked_sub(level + 2)?;
            let after = raw.get(open..)?;
            let newline = ["\r\n", "\n\r", "\n", "\r"].into_iter().find(|newline| after.starts_with(newline));
            let from = open + newline.map_or(0, str::len);
            for (index, ch) in raw.get(from..close)?.char_indices() {
                content.push(ch, span.start + (from + index) as u32);
            }
            content.offsets.push(span.start + close as u32);
            return Some(content);
        }
        let body = raw.get(1..raw.len().checked_sub(1)?)?;
        let mut chars = body.char_indices().peekable();
        while let Some((index, ch)) = chars.next() {
            let at = span.start + 1 + index as u32;
            if ch != '\\' {
                content.push(ch, at);
                continue;
            }
            let Some((_, escaped)) = chars.next() else { break };
            match escaped {
                'n' | '\n' => content.push('\n', at),
                '\r' => {
                    chars.next_if(|(_, next)| *next == '\n');
                    content.push('\n', at);
                }
                't' => content.push('\t', at),
                'r' => content.push('\r', at),
                'a' => content.push('\x07', at),
                'b' => content.push('\x08', at),
                'f' => content.push('\x0c', at),
                'v' => content.push('\x0b', at),
                'z' => {
                    while chars.peek().is_some_and(|(_, next)| next.is_whitespace()) {
                        chars.next();
                    }
                }
                // A byte written as `\x41` or `\65`, or a character as `\u{48}`, stands for itself.
                'x' | 'u' | '0'..='9' => return None,
                other => content.push(other, at),
            }
        }
        content.offsets.push(span.end - 1);
        Some(content)
    }

    fn push(&mut self, ch: char, at: u32) {
        self.text.push(ch);
        self.offsets.extend(std::iter::repeat_n(at, ch.len_utf8()));
    }

    /// The offset in the source of the character at `line` and `column` of the text, counted from 1.
    fn offset(&self, (line, column): (usize, usize)) -> Option<u32> {
        let mut start = 0;
        for _ in 1..line {
            start += self.text.get(start..)?.find('\n')? + 1;
        }
        let rest = self.text.get(start..)?;
        let index = rest.char_indices().nth(column.checked_sub(1)?).map_or(rest.len(), |(index, _)| index);
        self.offsets.get(start + index).copied()
    }
}
