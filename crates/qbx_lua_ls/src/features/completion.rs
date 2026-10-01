use std::sync::Arc;

use lsp_types::{
    Command, CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionItemTag, CompletionList,
    CompletionResponse, Documentation, InsertTextFormat, Position, Range, TextEdit,
};
use qbx_fivem_data::{native, native_docs, natives, Side};
use qbx_lua_analysis::manifest::KNOWN_DIRECTIVES;
use qbx_lua_analysis::project::relative_slash_path;
use qbx_lua_analysis::scope::LocalKind;
use qbx_lua_fmt::QuoteStyle;
use qbx_lua_syntax::ast::{Expr, ExprKind, Name};
use qbx_lua_syntax::{CommentKind, SmolStr, Span, TokenKind};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::json;

use super::class_tables::{class_table_at, named_field, Classes};
use super::expected::{expected_type, Place};
use super::hover::{event_handler_signature, event_string_context};
use super::visibility::Scope;
use super::{lua_block, markdown, with_infer};
use crate::callback_wrappers::{families, takes_function, Wrapper};
use crate::document::Document;
use crate::index::{EventFamily, EventKind, FileOrigin, SymbolKind};
use crate::infer::{native_fun_type, Infer, MemberInfo};
use crate::locate::{locate, string_content_span};
use crate::types::{CallbackRole, DescribedValue, FunType, Param, Type};
use crate::workspace::Workspace;

const MAX_NATIVES: usize = 120;
const MAX_ITEMS: usize = 600;

const KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
    "not", "or", "repeat", "return", "then", "true", "until", "while",
];

const THREAD_LOOP: &str = "CreateThread(function()\n\twhile true do\n\t\t$0\n\t\tWait(${1:0})\n\tend\nend)";

/// Strings are written with `'`, which [`quoted`] swaps for the quote of the document.
const SNIPPETS: &[(&str, &str, &str)] = &[
    ("CreateThread", THREAD_LOOP, "Thread with a loop that yields every iteration"),
    ("thread", THREAD_LOOP, "Thread with a loop that yields every iteration"),
    ("CreateThread once", "CreateThread(function()\n\t$0\nend)", "Thread that runs its body once"),
    ("SetTimeout", "SetTimeout(${1:1000}, function()\n\t$0\nend)", "Run a function after a delay"),
    (
        "RegisterNetEvent",
        "RegisterNetEvent('${1:resource}:${2:event}', function(${3})\n\t$0\nend)",
        "Register a network event with a handler",
    ),
    ("AddEventHandler", "AddEventHandler('${1:eventName}', function(${2})\n\t$0\nend)", "Handle a local event"),
    (
        "RegisterCommand",
        "RegisterCommand('${1:name}', function(source, args, raw)\n\t$0\nend, ${2:false})",
        "Register a console/chat command",
    ),
    (
        "lib.callback.register",
        "lib.callback.register('${1:resource}:${2:name}', function(source${3})\n\t$0\nend)",
        "Register an ox_lib server callback",
    ),
    ("lib.callback.await", "lib.callback.await('${1:resource}:${2:name}', ${3:false}$0)", "Await an ox_lib callback"),
    ("for pairs", "for ${1:k}, ${2:v} in pairs(${3:t}) do\n\t$0\nend", "Iterate over a table"),
    ("for ipairs", "for ${1:i}, ${2:v} in ipairs(${3:t}) do\n\t$0\nend", "Iterate over an array"),
    ("for i", "for ${1:i} = ${2:1}, ${3:#t} do\n\t$0\nend", "Numeric for loop"),
    ("function", "function ${1:name}(${2})\n\t$0\nend", "Function declaration"),
    ("local function", "local function ${1:name}(${2})\n\t$0\nend", "Local function declaration"),
    ("if", "if ${1:condition} then\n\t$0\nend", "If statement"),
    ("while", "while ${1:condition} do\n\t$0\nend", "While loop"),
];

const DOC_TAGS: &[(&str, &str)] = &[
    ("param", "param ${1:name} ${2:type}"),
    ("return", "return ${1:type}"),
    ("type", "type ${1:type}"),
    ("class", "class ${1:Name}"),
    ("field", "field ${1:name} ${2:type}"),
    ("alias", "alias ${1:Name} ${2:type}"),
    ("enum", "enum ${1:Name}"),
    ("generic", "generic ${1:T}"),
    ("overload", "overload fun(${1}): ${2:any}"),
    ("deprecated", "deprecated"),
    ("async", "async"),
    ("nodiscard", "nodiscard"),
    ("meta", "meta"),
    ("diagnostic", "diagnostic disable-next-line: ${1:undefined-global}"),
    ("callback", "callback ${1|register,await,trigger|}"),
    ("see", "see ${1:symbol}"),
];

const PRIMITIVE_TYPES: &[&str] = &[
    "any",
    "nil",
    "boolean",
    "number",
    "integer",
    "string",
    "table",
    "function",
    "thread",
    "userdata",
    "unknown",
    "fun()",
    "table<string, any>",
];

const REQUIRE_CALLS: &[&str] = &["require", "lib.require", "lib.load"];
const RESOURCE_NAME_CALLS: &[&str] = &[
    "GetResourceState",
    "StartResource",
    "StopResource",
    "GetResourcePath",
    "GetResourceMetadata",
    "LoadResourceFile",
];

fn kind_of(kind: SymbolKind, ty: &Type) -> CompletionItemKind {
    match kind {
        SymbolKind::Function | SymbolKind::Export => CompletionItemKind::FUNCTION,
        SymbolKind::Method => CompletionItemKind::METHOD,
        SymbolKind::Class => CompletionItemKind::CLASS,
        SymbolKind::Alias => CompletionItemKind::INTERFACE,
        SymbolKind::Table => CompletionItemKind::MODULE,
        _ if ty.as_fun().is_some() => CompletionItemKind::FUNCTION,
        SymbolKind::Field => CompletionItemKind::FIELD,
        SymbolKind::Variable => CompletionItemKind::VARIABLE,
    }
}

fn detail_of(name: &str, ty: &Type) -> Option<String> {
    match ty {
        Type::Unknown => None,
        Type::Fun(fun) => Some(fun.signature(name)),
        Type::GlobalTable(path) if path.starts_with('%') => Some("table".into()),
        other => Some(other.to_string()),
    }
}

fn item(label: &str, kind: CompletionItemKind, sort_group: u8) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        sort_text: Some(format!("{sort_group}{label}")),
        ..CompletionItem::default()
    }
}

/// Snippets sort ahead of the plain name they share a label with, otherwise accepting the first
/// `CreateThread` would only insert the word.
fn snippet_item(label: &str, body: &str, description: &str) -> CompletionItem {
    let is_statement = body.starts_with(|c: char| c.is_ascii_lowercase()) && !body.starts_with("lib.");
    let kind = if is_statement { CompletionItemKind::KEYWORD } else { CompletionItemKind::EVENT };
    let mut out = item(label, kind, 0);
    out.sort_text = Some(format!("/{label}"));
    out.filter_text = Some(label.to_string());
    out.insert_text = Some(body.to_string());
    out.insert_text_format = Some(InsertTextFormat::SNIPPET);
    out.detail = Some(description.to_string());
    out.label_details = Some(CompletionItemLabelDetails { detail: None, description: Some("snippet".to_string()) });
    out.documentation = Some(Documentation::MarkupContent(markdown(lua_block(&snippet_preview(body)))));
    out
}

/// What the string values a parameter lists mean for a call snippet.
#[derive(Clone, Copy, Default, PartialEq)]
enum Listed {
    #[default]
    Nothing,
    Values,
    /// An `@overload` takes one of them alone, so what the rest of the call passes depends on it, as
    /// the handler of `OnAction("playerUnloaded", handler)` does.
    Deciding,
}

/// A call snippet, and whether its first stop is where the values of a list or the registered
/// callback names can be suggested.
struct CallSnippet {
    body: String,
    lists_first: bool,
}

/// A call to `name` with its callbacks written out, for functions that take one, such as
/// `TriggerCallback('${1:event}', function(${2:...})\n\t$0\nend)`. Arguments after the last callback
/// that may be left out, such as a `...` payload, share one stop after its `end`.
///
/// `lookup` is the parameter an `await` or `trigger` wrapper takes a callback name in. Its first
/// stop is left empty between the quotes, where the registered names can be suggested. So are the
/// stops of parameters that list string values, which `listed` gives by position. A parameter whose
/// value decides the rest of the call ends the snippet, with the final stop right after it, since
/// the arguments that follow can only be written out once it is picked.
fn call_snippet(
    name: &str,
    fun: &FunType,
    via_colon: bool,
    lookup: Option<usize>,
    listed: &[Listed],
    quote: char,
) -> Option<CallSnippet> {
    let (skip_params, _) = fun.call_offsets(via_colon);
    let params = fun.params.get(skip_params..)?;
    let listed_at = |i: usize| listed.get(i).copied().unwrap_or_default();
    let required = |p: &Param| p.name != "..." && !p.optional;
    let deciding = match lookup {
        Some(_) => None,
        None => (0..params.len()).find(|&i| required(&params[i]) && listed_at(i) == Listed::Deciding),
    };
    let last_callback = params.iter().rposition(|p| p.name != "..." && takes_function(&p.ty));
    let written = match deciding {
        Some(deciding) => &params[..=deciding],
        None => {
            let last_listed =
                (0..params.len()).rev().find(|&i| required(&params[i]) && listed_at(i) != Listed::Nothing);
            let last = last_callback.max(lookup).max(last_listed)?;
            let required_after = params[last + 1..].iter().take_while(|p| required(p)).count();
            &params[..=last + required_after]
        }
    };
    // The callback whose body holds the final stop, unless that comes after a deciding value.
    let final_callback = last_callback.filter(|_| deciding.is_none());
    let mut stop = usize::from(lookup.is_some());
    let mut next = || {
        stop += 1;
        stop
    };
    let mut lists_first = lookup.is_some();
    let mut args = Vec::new();
    for (i, param) in written.iter().enumerate() {
        if Some(i) == lookup {
            args.push(format!("{quote}$1{quote}"));
            continue;
        }
        if listed_at(i) != Listed::Nothing {
            let stop = next();
            lists_first |= stop == 1;
            args.push(format!("{quote}${stop}{quote}"));
            continue;
        }
        if last_callback.is_none_or(|last| i > last) || !takes_function(&param.ty) {
            let is_string = matches!(param.ty.without_nil(), Type::String | Type::StringLit(_));
            let quote = if is_string { quote.to_string() } else { String::new() };
            args.push(format!("{quote}${{{}:{}}}{quote}", next(), param.name));
            continue;
        }
        let callback_params = match param.ty.as_fun() {
            Some(callback) if callback.params.is_empty() => String::new(),
            Some(callback) => {
                let names: Vec<&str> = callback.params.iter().map(|p| p.name.as_str()).collect();
                format!("${{{}:{}}}", next(), names.join(", "))
            }
            None => format!("${{{}}}", next()),
        };
        let body = if Some(i) == final_callback { "$0".to_string() } else { format!("${}", next()) };
        args.push(format!("function({callback_params})\n\t{body}\nend"));
    }
    let rest = match deciding {
        Some(_) => "$0".to_string(),
        None if params.len() > written.len() => format!("${}", next()),
        None => String::new(),
    };
    Some(CallSnippet { body: format!("{name}({}{rest})", args.join(", ")), lists_first })
}

/// How call snippets are offered.
#[derive(Clone, Copy)]
struct CallSnippets {
    /// The client runs `editor.action.triggerSuggest` for us, so inserting a call can list the values
    /// or callback names of its first stop right away.
    reopen_suggestions: bool,
    /// The quote of the strings the snippets write.
    quote: char,
    /// Where the call is written, which decides the `@overload`s that apply.
    at: u32,
}

/// The call snippet of a completed function, beside the item that inserts only its name.
fn call_snippet_item(
    infer: &Infer,
    name: &str,
    ty: &Type,
    via_colon: bool,
    options: CallSnippets,
) -> Option<CompletionItem> {
    let fun = ty.as_fun()?;
    // `Shop.price(` has to pass `self` itself; the snippet would leave it out.
    if fun.is_method && !via_colon {
        return None;
    }
    // A name to register is new, so only `await` and `trigger` wrappers look one up.
    let wrapper = Wrapper::of(fun, via_colon).filter(|wrapper| wrapper.tag.role != CallbackRole::Register);
    let lookup = wrapper.map(|wrapper| wrapper.name);
    let signatures = infer.open_call_signatures(fun, &[], via_colon, options.at);
    let listed: Vec<Listed> = (0..fun.params.len())
        .map(|i| {
            // The stop of a listed value is written between quotes, which only strings take.
            let mut literals = argument_literals(infer, &signatures, i, via_colon, "");
            literals.retain(|literal| literal.string().is_some());
            match literals.iter().any(|literal| !literal.taken_alone_by.is_empty()) {
                true => Listed::Deciding,
                false if literals.is_empty() => Listed::Nothing,
                false => Listed::Values,
            }
        })
        .collect();
    let snippet = call_snippet(name, fun, via_colon, lookup, &listed, options.quote)?;
    let mut out = snippet_item(name, &snippet.body, &fun.signature(name));
    out.kind = Some(if via_colon { CompletionItemKind::METHOD } else { CompletionItemKind::FUNCTION });
    if snippet.lists_first && options.reopen_suggestions {
        let title = if lookup.is_some() { "Suggest callback names" } else { "Suggest values" };
        out.command = Some(Command::new(title.into(), "editor.action.triggerSuggest".into(), None));
    }
    Some(out)
}

/// A snippet written with `'` around its strings, with `quote` around them instead.
fn quoted(body: &str, quote: char) -> String {
    body.replace('\'', quote.encode_utf8(&mut [0; 4]))
}

/// Encodes a literal value inside a short Lua string without changing its contents.
fn escaped_string(value: &str, quote: char) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            ch if ch.is_ascii_control() => out.push_str(&format!("\\x{:02x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

/// The snippet as it looks right after insertion: `${1:0}` becomes `0`, `${1|a,b|}` becomes `a`.
pub fn snippet_preview(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        if let Some(inner) = rest.strip_prefix('{') {
            let end = inner.find('}').unwrap_or(inner.len());
            let placeholder = &inner[..end];
            let shown = match placeholder.split_once([':', '|']) {
                Some((_, default)) => default.split([',', '|']).next().unwrap_or_default(),
                None => "",
            };
            out.push_str(shown);
            rest = inner.get(end + 1..).unwrap_or_default();
        } else {
            rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        }
    }
    out.push_str(rest);
    out.replace('\t', "    ")
}

pub struct SnippetInfo {
    pub label: String,
    pub description: String,
    pub body: String,
}

/// Every snippet the server offers, for the editor's "show snippets" picker.
pub fn all_snippets(ws: &Workspace, doc: Option<&Document>) -> Vec<SnippetInfo> {
    let quote = quote_of(ws, doc);
    let mut out: Vec<SnippetInfo> = SNIPPETS
        .iter()
        .map(|(label, body, description)| SnippetInfo {
            label: label.to_string(),
            description: description.to_string(),
            body: quoted(body, quote),
        })
        .collect();
    let on_cache = match doc {
        Some(doc) => with_infer(ws, doc, |infer| on_cache_snippet(infer, "lib.", quote)),
        None => on_cache_item(Vec::new(), "lib.", quote),
    };
    out.push(SnippetInfo {
        label: on_cache.label,
        description: on_cache.detail.unwrap_or_default(),
        body: on_cache.insert_text.unwrap_or_default(),
    });
    out
}

fn member_item(member: &MemberInfo) -> CompletionItem {
    let mut out = item(&member.name, kind_of(member.kind, &member.ty), 0);
    out.detail = match (detail_of(&member.name, &member.ty), &member.literal) {
        (Some(ty), Some(value)) => Some(format!("{ty} = {value}")),
        (detail, _) => detail,
    };
    out.documentation = member.doc.as_ref().map(|d| Documentation::MarkupContent(markdown(d.to_string())));
    if member.deprecated {
        out.tags = Some(vec![CompletionItemTag::DEPRECATED]);
    }
    if !is_identifier(&member.name) {
        out.filter_text = Some(member.name.to_string());
    }
    out
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn identifier_prefix(before: &str) -> &str {
    let start = before.rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(0, |i| i + 1);
    &before[start..]
}

/// `reopen_suggestions`: the client runs `editor.action.triggerSuggest` when a completion asks it to.
/// `trigger_character`: the character whose typing asked for these completions, if any.
pub fn completion(
    ws: &Workspace,
    doc: &Document,
    position: Position,
    snippets: bool,
    reopen_suggestions: bool,
    trigger_character: Option<&str>,
) -> Option<CompletionResponse> {
    let offset = doc.offset(position);
    let line_start = doc.lines.line_start(position.line) as usize;
    let before = doc.text.get(line_start..offset as usize)?;

    let comment = doc.chunk.comments.iter().find(|c| c.span.start < offset && offset <= c.span.end);
    let in_string = doc.chunk.tokens.iter().position(|t| {
        let text = t.span.text(&doc.text);
        let unterminated = text.len() < 2 || text.as_bytes()[0] != text.as_bytes()[text.len() - 1];
        matches!(t.kind, TokenKind::String)
            && t.span.start < offset
            && (offset < t.span.end || (unterminated && offset == t.span.end))
    });

    let quote = quote_of(ws, Some(doc));
    let snippet_quote = snippets.then_some(quote);
    // A typed `(` or `,` only asks for what the argument after it takes, so that Enter after the `,`
    // of a table or any other list still inserts a newline.
    if matches!(trigger_character, Some("(" | ",")) {
        if comment.is_some() || in_string.is_some() {
            return None;
        }
        let items = with_infer(ws, doc, |infer| argument_items(infer, doc, offset, before, snippets, quote, true));
        return (!items.is_empty()).then(|| respond(items, false));
    }
    // A typed space only asks for the values that what follows it can be, so that no other space
    // opens a list. The list is incomplete, so a typed word asks again for the names in scope too.
    if trigger_character == Some(" ") {
        if comment.is_some() || in_string.is_some() {
            return None;
        }
        let items = value_items(ws, doc, offset, before, snippets, quote);
        return (!items.is_empty()).then(|| respond(items, true));
    }

    if let Some(comment) = comment {
        let is_doc = comment.kind == CommentKind::Line && comment.span.text(&doc.text).starts_with("---");
        return is_doc.then(|| respond(doc_comment_items(ws, before, snippets), false));
    }

    if let Some(token_index) = in_string {
        return Some(respond(string_items(ws, doc, offset, token_index), false));
    }

    let prefix = identifier_prefix(before);
    let head = before[..before.len() - prefix.len()].trim_end();
    if prefix.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    // A call snippet would repeat a `(` that already follows the name, and has no place in
    // `function name`.
    let after = doc.text[offset as usize..].trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    let statement = head.trim_start();
    let defines = matches!(statement, "function" | "local function") || statement.starts_with("function ");
    let call_snippets = (snippets && !after.starts_with('(') && !defines).then_some(CallSnippets {
        reopen_suggestions,
        quote,
        at: offset,
    });

    if (head.ends_with('.') && !head.ends_with("..")) || (head.ends_with(':') && !head.ends_with("::")) {
        let via_colon = head.ends_with(':');
        let mut items = with_infer(ws, doc, |infer| {
            member_items(infer, doc, offset, head, via_colon, snippet_quote, call_snippets)
        });
        let base = head[..head.len() - 1].trim_end();
        if !via_colon && (base.ends_with(".state") || base.ends_with("GlobalState")) {
            items.extend(state_key_items(ws));
        }
        return Some(respond(items, false));
    }

    if doc.is_manifest() {
        return Some(respond(manifest_items(before, prefix, snippet_quote), false));
    }

    if head.ends_with('{') || head.ends_with(',') || head.is_empty() {
        let fields = with_infer(ws, doc, |infer| expected_field_items(infer, doc, offset));
        if !fields.is_empty() {
            return Some(respond(fields, false));
        }
    }
    let mut items = with_infer(ws, doc, |infer| argument_items(infer, doc, offset, before, snippets, quote, false));
    items.extend(value_items(ws, doc, offset, before, snippets, quote));
    if prefix.is_empty() {
        return (!items.is_empty()).then(|| respond(items, false));
    }
    let (mut scope, incomplete) =
        with_infer(ws, doc, |infer| scope_items(ws, infer, doc, offset, prefix, snippet_quote, call_snippets));
    // `true`, `false` and `nil` are listed once, as the values they are.
    let is_keyword = |item: &CompletionItem| item.kind == Some(CompletionItemKind::KEYWORD);
    scope.retain(|name| !is_keyword(name) || !items.iter().any(|value| is_keyword(value) && value.label == name.label));
    items.extend(scope);
    Some(respond(items, incomplete))
}

/// What the argument that the cursor starts can be, right after the `(` or `,` in front of it: the
/// values its parameter lists, the strings quoted, and a function literal when it takes one, such
/// as `function(${1:source})\n\t$0\nend` after `OnAction("playerUnloaded", `. Both come from the
/// signatures that fit the arguments before it, the function from those that fit them best, with one
/// item for each list of parameters they give it. Function literals are snippets.
///
/// `typed`: the `(` or `,` was just typed. That lists the values of a parameter that names strings
/// or integers, and not the `true` and `false` that so many parameters take, which are left to a
/// request for them.
fn argument_items(
    infer: &Infer,
    doc: &Document,
    offset: u32,
    before: &str,
    snippets: bool,
    quote: char,
    typed: bool,
) -> Vec<CompletionItem> {
    let prefix = identifier_prefix(before);
    let head = before[..before.len() - prefix.len()].trim_end();
    let Some(punctuation) = head.chars().last().filter(|c| matches!(c, '(' | ',')) else { return Vec::new() };
    // Text after the argument would end up behind the inserted value.
    let line_end = doc.text[offset as usize..].find('\n').map_or(doc.text.len(), |i| offset as usize + i);
    let rest = doc.text[offset as usize..line_end].trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    if !(rest.trim().is_empty() || rest.trim_start().starts_with([')', ','])) {
        return Vec::new();
    }
    let Some(site) = locate(&doc.chunk, offset).call else { return Vec::new() };
    // The `(` or `,` has to separate the arguments of the call, not those of a table or a
    // parenthesized expression inside one.
    let at = offset - (before.len() - head.len()) as u32 - 1;
    let separates_arguments = match punctuation {
        '(' => site.args_span.start == at,
        _ => site.args_span.start < at && !site.args.iter().any(|arg| arg.span.start <= at && at < arg.span.end),
    };
    if !separates_arguments {
        return Vec::new();
    }
    let Some((fun, _)) = infer.callee_fun(site.base, site.method) else { return Vec::new() };
    let via_method = site.method.is_some();
    let argument = site.active_argument(&doc.text, offset);
    let args = &site.args[..argument.min(site.args.len())];
    let space = if before[..before.len() - prefix.len()].ends_with(',') { " " } else { "" };
    let signatures = infer.open_call_signatures(&fun, args, via_method, site.base.span.start);
    let name = callee_name(site.base, site.method);
    let mut literals = argument_literals(infer, &signatures, argument, via_method, &name);
    if typed && !literals.iter().any(|literal| matches!(literal.value, Type::StringLit(_) | Type::IntLit(_))) {
        literals.clear();
    }
    let mut items: Vec<CompletionItem> =
        literals.iter().enumerate().map(|(i, literal)| literal.written_item(i, quote, space)).collect();
    if !snippets {
        return items;
    }
    let mut seen = FxHashSet::default();
    for (signature, best) in signatures {
        let Some(param) = best.then(|| param_for_argument(&signature, argument, via_method)).flatten() else {
            continue;
        };
        let Some(out) = function_literal_item(&param.ty, &param.to_string(), space) else { continue };
        if seen.insert(out.insert_text.clone()) {
            items.push(out);
        }
    }
    items
}

/// A function literal for a value of type `ty` that takes a function: `function(source) ... end`
/// with the parameters `ty` names, written after `space`. `description` is shown beside it.
fn function_literal_item(ty: &Type, description: &str, space: &str) -> Option<CompletionItem> {
    if !takes_function(ty) {
        return None;
    }
    let names = ty.as_fun().map(|callback| {
        let names: Vec<&str> = callback.params.iter().map(|p| p.name.as_str()).collect();
        names.join(", ")
    });
    let stop = match names.as_deref() {
        Some("") => String::new(),
        Some(names) => format!("${{1:{names}}}"),
        None => "$1".to_string(),
    };
    let body = format!("{space}function({stop})\n\t$0\nend");
    let label = format!("function({})", names.unwrap_or_default());
    let mut out = snippet_item(&label, &body, description);
    out.kind = Some(CompletionItemKind::FUNCTION);
    // Ahead of the `function name()` declaration snippet too.
    out.sort_text = Some(format!(".{label}"));
    out.filter_text = Some("function".to_string());
    Some(out)
}

/// What the value that the cursor starts can be, right after the `=`, `==`, `~=` or `return` in
/// front of it: the values that its type lists, the strings quoted, such as `"busy"` and `"ready"`
/// after `local state = ` under a `---@type "busy"|"ready"`, and a function literal where a
/// function is stored or returned. The type is the one [`expected_type`] finds. Function literals
/// are snippets.
fn value_items(
    ws: &Workspace,
    doc: &Document,
    offset: u32,
    before: &str,
    snippets: bool,
    quote: char,
) -> Vec<CompletionItem> {
    let prefix = identifier_prefix(before);
    let typed = &before[..before.len() - prefix.len()];
    let head = typed.trim_end();
    let line_end = doc.text[offset as usize..].find('\n').map_or(doc.text.len(), |i| offset as usize + i);
    if !takes_value(head) || !leaves_value_open(&doc.text[offset as usize..line_end]) {
        return Vec::new();
    }
    let at = offset - (before.len() - head.len()) as u32;
    let space = if typed.len() == head.len() { " " } else { "" };
    with_infer(ws, doc, |infer| {
        let Some(expected) = expected_type(infer, &doc.chunk, Place::After(at)) else { return Vec::new() };
        let values = listed_values(infer, &expected.ty, &expected.values);
        let mut items: Vec<CompletionItem> =
            values.iter().enumerate().map(|(i, value)| value.written_item(i, quote, space)).collect();
        if snippets && !expected.compared {
            let ty = infer.resolve_alias(&expected.ty.without_nil());
            items.extend(function_literal_item(&ty, &expected.ty.to_string(), space));
        }
        items
    })
}

/// Whether `head`, the text in front of a value, ends in what takes one: `=`, `==`, `~=` or `return`.
fn takes_value(head: &str) -> bool {
    match head.strip_suffix("return") {
        Some(before) => !before.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_'),
        None => head.ends_with('=') && !head.ends_with("<=") && !head.ends_with(">="),
    }
}

/// Whether `rest`, the text after the cursor on its line, has no value that an inserted one would
/// end up in front of: it is empty, a comment, or what closes or follows a value, like the `then`
/// of `if state == then`. A word that starts right at the cursor is a value already written.
fn leaves_value_open(rest: &str) -> bool {
    if rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let rest = rest.trim_start();
    let word = rest.trim_start_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
    let word = &rest[..rest.len() - word.len()];
    rest.is_empty()
        || rest.starts_with([')', '}', ']', ',', ';'])
        || rest.starts_with("--")
        || matches!(word, "then" | "do" | "and" | "or" | "end" | "else" | "elseif" | "until")
}

/// The quote that inserted strings use: the formatter's `quote_style`, or else the one that most
/// strings of the document use, and `'` when it has none or there is no document.
pub fn quote_of(ws: &Workspace, doc: Option<&Document>) -> char {
    match ws.lint_config.format.quote_style {
        QuoteStyle::Single => return '\'',
        QuoteStyle::Double => return '"',
        QuoteStyle::Preserve => {}
    }
    let Some(doc) = doc else { return '\'' };
    let (mut single, mut double) = (0, 0);
    for token in doc.chunk.tokens.iter().filter(|t| t.kind == TokenKind::String) {
        match doc.text.as_bytes().get(token.span.start as usize) {
            Some(b'\'') => single += 1,
            Some(b'"') => double += 1,
            _ => {}
        }
    }
    if double > single {
        '"'
    } else {
        '\''
    }
}

/// The name a call's signatures are shown under: `OnAction`, or the `on` of `emitter:on`.
fn callee_name(base: &Expr, method: Option<&Name>) -> String {
    match method {
        Some(method) => method.text.to_string(),
        None => base.dotted_path().map(|path| path.to_string()).unwrap_or_default(),
    }
}

/// The parameter an argument at `arg_index` of a call is passed to.
fn param_for_argument(fun: &FunType, arg_index: usize, via_method: bool) -> Option<&Param> {
    let (skip_params, skip_args) = fun.call_offsets(via_method);
    (arg_index + skip_params).checked_sub(skip_args).and_then(|i| fun.params.get(i))
}

fn respond(mut items: Vec<CompletionItem>, incomplete: bool) -> CompletionResponse {
    let truncated = items.len() > MAX_ITEMS;
    items.truncate(MAX_ITEMS);
    CompletionResponse::List(CompletionList { is_incomplete: incomplete || truncated, items })
}

fn doc_comment_items(ws: &Workspace, before: &str, snippets: bool) -> Vec<CompletionItem> {
    let Some(at) = before.rfind("---") else { return Vec::new() };
    let content = before[at + 3..].trim_start();
    if let Some(tag_prefix) = content.strip_prefix('@').filter(|rest| !rest.contains(char::is_whitespace)) {
        return DOC_TAGS
            .iter()
            .filter(|(tag, _)| tag.starts_with(tag_prefix))
            .map(|(tag, snippet)| {
                let mut out = item(tag, CompletionItemKind::KEYWORD, 0);
                out.insert_text =
                    Some(if snippets { snippet.to_string() } else { tag.trim_start_matches('@').to_string() });
                if snippets {
                    out.insert_text_format = Some(InsertTextFormat::SNIPPET);
                }
                out
            })
            .collect();
    }
    if let Some(rest) = content.strip_prefix("@callback").filter(|rest| rest.starts_with(char::is_whitespace)) {
        return callback_tag_items(ws, rest);
    }
    let takes_type = ["@type", "@return", "@param", "@field", "@alias", "@class", "@overload", "@generic", "|"]
        .iter()
        .any(|tag| content.starts_with(tag));
    if !takes_type {
        return Vec::new();
    }
    let mut items: Vec<CompletionItem> =
        PRIMITIVE_TYPES.iter().map(|t| item(t, CompletionItemKind::KEYWORD, 1)).collect();
    let mut seen = FxHashSet::default();
    for name in ws.index.class_names().filter(|n| seen.insert((*n).clone())) {
        items.push(item(name, CompletionItemKind::CLASS, 0));
    }
    items
}

const CALLBACK_ROLES: &[(&str, &str)] = &[
    ("register", "Registers a handler under a name, like lib.callback.register"),
    ("await", "Runs the handler of a name and returns its response, like lib.callback.await"),
    ("trigger", "Runs the handler of a name and passes its response to a function"),
];

/// After `---@callback`, its role, then a family name that other wrappers already use.
fn callback_tag_items(ws: &Workspace, rest: &str) -> Vec<CompletionItem> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    let typing = if rest.ends_with(char::is_whitespace) { words.len() } else { words.len().saturating_sub(1) };
    match typing {
        0 => CALLBACK_ROLES
            .iter()
            .map(|(role, description)| {
                let mut out = item(role, CompletionItemKind::ENUM_MEMBER, 0);
                out.detail = Some(description.to_string());
                out
            })
            .collect(),
        // The word being typed is already the family of the wrapper this comment tags.
        1 if CALLBACK_ROLES.iter().any(|(role, _)| *role == words[0]) => families(&ws.index)
            .iter()
            .filter(|family| words.get(1) != Some(&family.as_str()))
            .map(|family| {
                let mut out = item(family, CompletionItemKind::MODULE, 0);
                out.detail = Some("callback family".to_string());
                out
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn string_items(ws: &Workspace, doc: &Document, offset: u32, token_index: usize) -> Vec<CompletionItem> {
    let tokens = &doc.chunk.tokens;
    let indexes_exports = token_index >= 2
        && tokens[token_index - 1].kind == TokenKind::LBracket
        && tokens[token_index - 2].span.text(&doc.text) == "exports";
    if indexes_exports {
        return resource_items(ws, doc);
    }
    if doc.is_manifest() {
        return manifest_path_items(ws, doc);
    }

    let located = locate(&doc.chunk, offset);
    let Some((string, argument)) = located.string else { return Vec::new() };
    // Event names and literal values contain punctuation. Give clients the whole string content so a
    // `:` retrigger keeps filtering from the opening quote and accepting does not duplicate it.
    let token = &tokens[token_index];
    let content =
        string_content_span(token.span, &doc.text).unwrap_or_else(|| Span::new(token.span.start + 1, token.span.end));
    let range = doc.range(content);
    let quote = if token.span.text(&doc.text).starts_with('"') { '"' } else { '\'' };
    // A string that is no argument lists the values that its place takes.
    let Some((call, arg_index)) = argument else {
        return with_infer(ws, doc, |infer| {
            let Some(expected) = expected_type(infer, &doc.chunk, Place::String(string.span)) else {
                return Vec::new();
            };
            let values = listed_values(infer, &expected.ty, &expected.values);
            values.iter().enumerate().filter_map(|(i, value)| value.content_item(i, range, quote)).collect()
        });
    };
    let path = match &call.kind {
        ExprKind::Call { callee, .. } => callee.dotted_path(),
        _ => None,
    };
    let context = with_infer(ws, doc, |infer| event_string_context(infer, Some((call, arg_index))));
    if let Some(context) = context {
        if !context.active {
            return Vec::new();
        }
        let wants_callbacks = context.family != EventFamily::Native;
        let target_side = context.target_side;
        let handled_on_target = |side: Option<Side>| !matches!((target_side, side), (Some(target), Some(side)) if !side.is_available_on(target));
        let mut conflicting = FxHashSet::default();
        if context.framework() {
            let mut payloads = FxHashMap::default();
            for (_, event) in ws.index.events().filter(|(_, event)| context.accepts_registration(event)) {
                let signature = event_handler_signature(event);
                if let Some(previous) = payloads.get(&event.name) {
                    if previous != &signature {
                        conflicting.insert(event.name.clone());
                    }
                } else {
                    payloads.insert(event.name.clone(), signature);
                }
            }
        }
        let candidates = |strict: bool| {
            let mut seen = FxHashSet::default();
            ws.index
                .events()
                .filter(|(_, event)| event.family == context.family)
                .filter(|(_, e)| (e.kind == EventKind::Callback) == wants_callbacks || e.kind == EventKind::Trigger)
                .filter(|(_, e)| !strict || (e.kind != EventKind::Trigger && handled_on_target(e.side)))
                .filter(|(_, event)| !context.framework() || context.accepts_registration(event))
                .filter(|(_, e)| seen.insert(e.name.clone()))
                .map(|(file, event)| {
                    let mut out = item(&event.name, CompletionItemKind::EVENT, 0);
                    if range.start.line == range.end.line {
                        out.text_edit = Some(TextEdit { range, new_text: event.name.to_string() }.into());
                    }
                    let entry = ws.index.file(file);
                    let origin = entry.and_then(|f| f.resource).and_then(|r| ws.index.resource(r));
                    let side = event.side.map_or(String::new(), |s| format!(" ({})", s.label()));
                    out.detail = if conflicting.contains(&event.name) {
                        Some("Multiple handlers; payloads differ".into())
                    } else {
                        match (event_handler_signature(event), origin) {
                            (Some(handler), Some(resource)) => Some(format!("{}{side} · {handler}", resource.name)),
                            (None, Some(resource)) => Some(format!("{}{side}", resource.name)),
                            (Some(handler), None) => Some(handler),
                            (None, None) => None,
                        }
                    };
                    out
                })
                .collect::<Vec<_>>()
        };
        return candidates(target_side.is_some());
    }
    let literals = || with_infer(ws, doc, |infer| literal_items(infer, call, arg_index, range, quote));
    let Some(path) = path else { return literals() };
    let path = path.as_str();
    if arg_index == 0 && matches!(path, "lib.onCache") {
        return cache_key_items(ws, doc);
    }
    if arg_index == 0 && path == "locale" {
        let locale = ws.index.resource_of(doc.file).and_then(|r| qbx_lua_analysis::locale::LocaleFile::load(&r.root));
        return locale
            .iter()
            .flat_map(|file| &file.keys)
            .map(|(key, _, text)| {
                let mut out = item(key, CompletionItemKind::TEXT, 0);
                out.detail = Some(text.clone());
                out
            })
            .collect();
    }
    if arg_index == 0 && crate::indexer::CONVAR_CALLS.contains(&path) {
        let mut seen = FxHashSet::default();
        let indexed = ws.index.files().flat_map(|(_, f)| f.index.convars.iter());
        return ws
            .cfg_convars
            .iter()
            .chain(indexed)
            .filter(|name| seen.insert((*name).clone()))
            .map(|name| item(name, CompletionItemKind::CONSTANT, 0))
            .collect();
    }
    if arg_index == 0 && path == "AddStateBagChangeHandler" {
        return state_key_items(ws);
    }
    if arg_index == 0 && REQUIRE_CALLS.contains(&path) {
        return module_items(ws, doc);
    }
    if arg_index == 0 && RESOURCE_NAME_CALLS.contains(&path) {
        return resource_items(ws, doc);
    }
    literals()
}

/// A value that an argument, or a value written elsewhere, can take.
struct ListedValue {
    /// A string, integer or boolean literal, or `nil`.
    value: Type,
    /// The type that lists it, such as the alias `Actions`: for an argument, that of the first
    /// parameter that does.
    ty: Type,
    /// The signatures that take it alone, which a call passing it picks.
    taken_alone_by: Vec<String>,
    /// What the `---|` line that lists it says about it, or empty.
    description: String,
}

impl ListedValue {
    /// The value when it is a string.
    fn string(&self) -> Option<&SmolStr> {
        match &self.value {
            Type::StringLit(value) => Some(value),
            _ => None,
        }
    }

    /// The item for the `index`th value, in the order the values are declared.
    fn item(&self, index: usize, label: &str) -> CompletionItem {
        // `true`, `false` and `nil` look the way they do among the names in scope.
        let kind = match self.value {
            Type::BooleanLit(_) | Type::Nil => CompletionItemKind::KEYWORD,
            _ => CompletionItemKind::ENUM_MEMBER,
        };
        let mut out = item(label, kind, 0);
        out.sort_text = Some(format!("{index:04}"));
        if !self.ty.is_literal() {
            out.detail = Some(self.ty.to_string());
        }
        let mut documentation = self.description.clone();
        if !self.taken_alone_by.is_empty() {
            let signatures = lua_block(&self.taken_alone_by.join("\n"));
            documentation = match documentation.is_empty() {
                true => signatures,
                false => format!("{documentation}\n\n{signatures}"),
            };
        }
        if !documentation.is_empty() {
            out.documentation = Some(Documentation::MarkupContent(markdown(documentation)));
        }
        out
    }

    /// The item that writes the value, a string in quotes, after `space`, where no quote is typed
    /// yet.
    fn written_item(&self, index: usize, quote: char, space: &str) -> CompletionItem {
        let (written, filter) = match self.string() {
            Some(value) => (format!("{quote}{}{quote}", escaped_string(value, quote)), value.to_string()),
            None => (self.value.to_string(), self.value.to_string()),
        };
        let mut out = self.item(index, &written);
        out.filter_text = Some(filter);
        out.insert_text = Some(format!("{space}{written}"));
        out
    }

    /// The item that replaces the contents of the string at `range` with the value, when the value
    /// is a string.
    fn content_item(&self, index: usize, range: Range, quote: char) -> Option<CompletionItem> {
        let value = self.string()?;
        let mut out = self.item(index, value);
        let value = escaped_string(value, quote);
        if range.start.line == range.end.line {
            out.text_edit = Some(TextEdit { range, new_text: value }.into());
        } else {
            out.insert_text = Some(value);
        }
        Some(out)
    }
}

/// The values that `ty` lists, in the order they are declared, described by `described_by`, the
/// `---|` lines under the annotation that declares `ty`, or else by those of its aliases.
fn listed_values(infer: &Infer, ty: &Type, described_by: &[DescribedValue]) -> Vec<ListedValue> {
    let values = infer.listed_literals(ty).into_iter();
    values
        .map(|value| {
            let description = described(described_by, &value).or_else(|| alias_value_description(infer, ty, &value, 0));
            let description = description.unwrap_or_default();
            ListedValue { value, ty: ty.clone(), taken_alone_by: Vec::new(), description }
        })
        .collect()
}

/// What the `---|` line of an alias in `ty` that lists `value` says about it.
fn alias_value_description(infer: &Infer, ty: &Type, value: &Type, depth: u32) -> Option<String> {
    if depth > 8 {
        return None;
    }
    match ty {
        Type::Named(name, _) if infer.index.class(name, infer.side()).is_none() => {
            let (_, alias) = infer.index.alias(name, infer.side())?;
            described(&alias.values, value).or_else(|| alias_value_description(infer, &alias.ty, value, depth + 1))
        }
        Type::Union(types) => types.iter().find_map(|part| alias_value_description(infer, part, value, depth + 1)),
        _ => None,
    }
}

/// The description of `value` among the values of `---|` lines.
fn described(values: &[DescribedValue], value: &Type) -> Option<String> {
    let listed = values.iter().find(|listed| listed.value == *value && !listed.description.is_empty());
    listed.map(|listed| listed.description.clone())
}

/// The values that the parameter of the argument at `arg_index` lists in `signatures`, in the
/// order they are declared, with the `nil` that any of them allows last: the `"playerLoaded"` of
/// `action: "playerLoaded"|"playerUnloaded"|string` or of an alias, and the `"keyPressed"` of an
/// `@overload` that takes `action: "keyPressed"`. The signatures that take a value alone are shown
/// under `name`.
fn argument_literals(
    infer: &Infer,
    signatures: &[(Arc<FunType>, bool)],
    arg_index: usize,
    via_method: bool,
    name: &str,
) -> Vec<ListedValue> {
    let mut literals: Vec<ListedValue> = Vec::new();
    for (signature, _) in signatures {
        let Some(param) = param_for_argument(signature, arg_index, via_method) else { continue };
        let pinned = infer.pinned_literal(&param.ty);
        let mut values = infer.listed_literals(&param.ty);
        // The type of `x any` keeps none of the values that the `---|` lines under it list.
        if values.is_empty() && !param.values.is_empty() {
            let listed = param.values.iter().map(|listed| listed.value.clone()).collect();
            values = infer.listed_literals(&Type::Union(listed));
        }
        for value in values {
            let index = match literals.iter().position(|known| known.value == value) {
                Some(index) => index,
                None => {
                    let ty = param.ty.clone();
                    let taken_alone_by = Vec::new();
                    literals.push(ListedValue { value: value.clone(), ty, taken_alone_by, description: String::new() });
                    literals.len() - 1
                }
            };
            if literals[index].description.is_empty() {
                let description =
                    described(&param.values, &value).or_else(|| alias_value_description(infer, &param.ty, &value, 0));
                literals[index].description = description.unwrap_or_default();
            }
            if pinned.as_ref() == Some(&value) {
                literals[index].taken_alone_by.push(signature.signature(name));
            }
        }
    }
    literals.sort_by_key(|literal| literal.value == Type::Nil);
    literals
}

/// The string values that the parameter of a string argument lists, each replacing its contents.
fn literal_items(infer: &Infer, call: &Expr, arg_index: usize, range: Range, quote: char) -> Vec<CompletionItem> {
    let (base, method, args) = match &call.kind {
        ExprKind::Call { callee, args, .. } => (callee.as_ref(), None, args),
        ExprKind::MethodCall { base, method, args, .. } => (base.as_ref(), Some(method), args),
        _ => return Vec::new(),
    };
    let Some((fun, _)) = infer.callee_fun(base, method) else { return Vec::new() };
    let via_method = method.is_some();
    let signatures = infer.open_call_signatures(&fun, &args[..arg_index.min(args.len())], via_method, base.span.start);
    argument_literals(infer, &signatures, arg_index, via_method, &callee_name(base, method))
        .iter()
        .enumerate()
        .filter_map(|(i, literal)| literal.content_item(i, range, quote))
        .collect()
}

/// State bag keys are plain strings that both sides must agree on, so every key seen anywhere is offered.
fn state_key_items(ws: &Workspace) -> Vec<CompletionItem> {
    let mut seen = FxHashSet::default();
    ws.index
        .files()
        .flat_map(|(_, f)| f.index.state_keys.iter())
        .filter(|key| seen.insert((*key).clone()))
        .map(|key| {
            let mut out = item(key, CompletionItemKind::FIELD, 1);
            out.detail = Some("state bag key".into());
            out
        })
        .collect()
}

/// The value fields of ox_lib's `cache` as seen from this file, read from the indexed ox_lib source.
fn cache_keys(infer: &Infer) -> Vec<MemberInfo> {
    let mut keys: Vec<MemberInfo> =
        infer.members(&infer.global_type("cache")).into_iter().filter(|m| m.ty.as_fun().is_none()).collect();
    keys.sort_by(|a, b| a.name.cmp(&b.name));
    keys
}

fn cache_key_items(ws: &Workspace, doc: &Document) -> Vec<CompletionItem> {
    with_infer(ws, doc, |infer| {
        cache_keys(infer)
            .iter()
            .map(|key| {
                let mut out = item(&key.name, CompletionItemKind::ENUM_MEMBER, 0);
                out.detail = detail_of(&key.name, &key.ty).map(|ty| format!("cache.{}: {ty}", key.name));
                out
            })
            .collect()
    })
}

/// What ox_lib caches on the client, for workspaces that do not contain ox_lib itself.
const DEFAULT_CACHE_KEYS: &[&str] = &["ped", "vehicle", "seat", "weapon", "playerId", "serverId", "coords"];

fn on_cache_snippet(infer: &Infer, prefix: &str, quote: char) -> CompletionItem {
    on_cache_item(cache_keys(infer).iter().map(|k| k.name.to_string()).collect(), prefix, quote)
}

fn on_cache_item(mut keys: Vec<String>, prefix: &str, quote: char) -> CompletionItem {
    if keys.is_empty() {
        keys = DEFAULT_CACHE_KEYS.iter().map(|k| k.to_string()).collect();
    }
    let body = format!(
        "{prefix}onCache({quote}${{1|{}|}}{quote}, function(${{2:value}}, ${{3:oldValue}})\n\t$0\nend)",
        keys.join(",")
    );
    snippet_item("onCache", &body, &format!("React to an ox_lib cache change ({})", keys.join(", ")))
}

/// The resources of the workspace, then those that only a declared exports type names.
fn resource_items(ws: &Workspace, doc: &Document) -> Vec<CompletionItem> {
    let mut seen = FxHashSet::default();
    let workspace = ws.index.resources.iter().map(|r| &r.name);
    let declared = ws.index.declared_exports(doc.file).into_iter().map(|(_, symbol)| &symbol.name);
    workspace
        .chain(declared)
        .filter(|name| seen.insert(*name))
        .map(|name| item(name, CompletionItemKind::MODULE, 0))
        .collect()
}

fn module_items(ws: &Workspace, doc: &Document) -> Vec<CompletionItem> {
    let Some(resource) = ws.index.resource_of(doc.file) else { return Vec::new() };
    resource
        .files
        .iter()
        .filter_map(|id| ws.index.file(*id))
        .filter(|f| f.path != doc.path)
        .map(|f| {
            let relative = relative_slash_path(&resource.root, &f.path);
            let module = relative.trim_end_matches(".lua").replace('/', ".");
            let mut out = item(&module, CompletionItemKind::FILE, 0);
            out.detail = Some(relative);
            out
        })
        .collect()
}

fn manifest_path_items(ws: &Workspace, doc: &Document) -> Vec<CompletionItem> {
    let Some(root) = doc.path.parent() else { return Vec::new() };
    let mut items: Vec<CompletionItem> = qbx_lua_analysis::lint::all_files(root)
        .into_iter()
        .filter(|f| !f.ends_with("fxmanifest.lua") && !f.starts_with('.'))
        .take(400)
        .map(|f| item(&f, CompletionItemKind::FILE, 1))
        .collect();
    for import in qbx_fivem_data::KNOWN_IMPORTS {
        items.push(item(import.path, CompletionItemKind::REFERENCE, 0));
    }
    let _ = ws;
    items
}

/// `snippet_quote`: the quote that snippet strings use, when the client takes snippets.
fn manifest_items(before: &str, prefix: &str, snippet_quote: Option<char>) -> Vec<CompletionItem> {
    if before.trim_start().len() != prefix.len() {
        return Vec::new();
    }
    KNOWN_DIRECTIVES
        .iter()
        .map(|directive| {
            let mut out = item(directive, CompletionItemKind::PROPERTY, 0);
            let Some(quote) = snippet_quote else { return out };
            let snippet = match *directive {
                "fx_version" => "fx_version '${1|cerulean,bodacious,adamant|}'".to_string(),
                "game" => "game '${1|gta5,rdr3|}'".to_string(),
                "lua54" | "use_experimental_fxv2_oal" => format!("{directive} 'yes'"),
                d if d.ends_with('s') && !matches!(d, "this_is_a_map") => format!("{d} {{\n\t'$0',\n}}"),
                d => format!("{d} '$0'"),
            };
            out.insert_text = Some(quoted(&snippet, quote));
            out.insert_text_format = Some(InsertTextFormat::SNIPPET);
            out
        })
        .collect()
}

/// `snippet_quote`: the quote that snippet strings use, when the client takes snippets.
fn member_items(
    infer: &Infer,
    doc: &Document,
    offset: u32,
    head: &str,
    via_colon: bool,
    snippet_quote: Option<char>,
    call_snippets: Option<CallSnippets>,
) -> Vec<CompletionItem> {
    let located = locate(&doc.chunk, offset);
    let base_type = match &located.member {
        Some(access) => infer.expr(access.base()),
        None => type_of_path(infer, &head[..head.len() - 1], offset),
    };
    // Members that their class keeps from the code here, like `---@field private`, are left out.
    let scope = Scope::new(infer, &doc.chunk);
    let base = located.member.as_ref().map(|access| access.base());
    let mut members = infer.members(&base_type);
    members.retain(|m| scope.allows(&base_type, base, &m.name, offset));
    let has_methods = members.iter().any(|m| m.ty.as_fun().is_some());
    let mut items = Vec::new();
    for m in members.iter().filter(|m| !via_colon || !has_methods || m.ty.as_fun().is_some()) {
        if !is_identifier(&m.name) {
            continue;
        }
        let mut out = member_item(m);
        let is_method = m.ty.as_fun().is_some_and(|f| f.is_method);
        if via_colon != is_method && m.ty.as_fun().is_some() {
            out.sort_text = Some(format!("1{}", m.name));
        }
        items.push(out);
        if let Some(options) = call_snippets {
            items.extend(call_snippet_item(infer, &m.name, &m.ty, via_colon, options));
        }
    }
    if let Some(quote) = snippet_quote.filter(|_| head == "lib.") {
        items.push(on_cache_snippet(infer, "", quote));
    }
    items
}

/// Fallback for member completion when the parser could not attach the trailing `.` to an expression.
fn type_of_path(infer: &Infer, text: &str, offset: u32) -> Type {
    let start = text.rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':'))).map_or(0, |i| i + 1);
    let mut segments = text[start..].split(['.', ':']).filter(|s| !s.is_empty());
    let Some(root) = segments.next() else { return Type::Unknown };
    let mut ty = match infer.ctx.resolution.lookup_local_at(root, offset) {
        Some(id) => infer.local_type_at(id, offset),
        None => infer.global_type(root),
    };
    for segment in segments {
        ty = infer.member(&ty, segment).map(|m| m.ty).unwrap_or_default();
    }
    ty
}

/// Field names of the table type a call expects, when the cursor is inside a table argument.
fn expected_field_items(infer: &Infer, doc: &Document, offset: u32) -> Vec<CompletionItem> {
    // A table typed as a class, by `---@type`, a parameter, an assignment, `@return` or the field
    // of another such table, offers the `@field`s it does not set yet and the code here may set.
    let scope = Scope::new(infer, &doc.chunk);
    if let Some(found) = class_table_at(infer, &doc.chunk, offset) {
        let ExprKind::Table(existing) = &found.table.kind else { return Vec::new() };
        let present: FxHashSet<&str> = existing.iter().filter_map(named_field).map(|(name, _)| name).collect();
        let class = Type::Named(found.class.clone(), Vec::new());
        return Classes::new(infer)
            .fields(&found.class, found.from)
            .into_iter()
            .filter(|field| !present.contains(field.name.as_str()) && is_identifier(&field.name))
            .filter(|field| scope.allows(&class, None, &field.name, offset))
            .map(|field| {
                let mut out = item(&field.name, CompletionItemKind::PROPERTY, 0);
                out.detail = Some(field.ty.to_string());
                out.documentation = field.doc.map(|doc| Documentation::MarkupContent(markdown(doc.to_string())));
                out.insert_text = Some(format!("{} = ", field.name));
                out
            })
            .collect();
    }
    let located = locate(&doc.chunk, offset);
    let Some((call, arg_index, table)) = located.table_in_call else { return Vec::new() };
    let ExprKind::Table(existing) = &table.kind else { return Vec::new() };
    let fun = match &call.kind {
        ExprKind::Call { callee, .. } => infer.callee_fun(callee, None),
        ExprKind::MethodCall { base, method, .. } => infer.callee_fun(base, Some(method)),
        _ => None,
    };
    let Some((fun, _)) = fun else { return Vec::new() };
    let Some(param) = param_for_argument(&fun, arg_index, matches!(call.kind, ExprKind::MethodCall { .. })) else {
        return Vec::new();
    };
    let present: FxHashSet<&str> = existing
        .iter()
        .filter_map(|f| match f {
            qbx_lua_syntax::ast::TableField::Named { name, .. } => Some(name.text.as_str()),
            _ => None,
        })
        .collect();
    let expected = param.ty.without_nil();
    infer
        .members(&expected)
        .iter()
        .filter(|m| !present.contains(m.name.as_str()) && is_identifier(&m.name))
        .filter(|m| scope.allows(&expected, None, &m.name, offset))
        .map(|m| {
            let mut out = member_item(m);
            out.kind = Some(CompletionItemKind::PROPERTY);
            out.insert_text = Some(format!("{} = ", m.name));
            out
        })
        .collect()
}

/// `snippet_quote`: the quote that snippet strings use, when the client takes snippets.
fn scope_items(
    ws: &Workspace,
    infer: &Infer,
    doc: &Document,
    offset: u32,
    prefix: &str,
    snippet_quote: Option<char>,
    call_snippets: Option<CallSnippets>,
) -> (Vec<CompletionItem>, bool) {
    let matches = |name: &str| name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix);
    let mut items = Vec::new();
    let mut seen: FxHashSet<String> = FxHashSet::default();
    // `CreateThread` and the like have a hand-written snippet already.
    let call_snippet = |name: &str, ty: &Type| {
        let options = call_snippets.filter(|_| !SNIPPETS.iter().any(|(label, ..)| *label == name))?;
        call_snippet_item(infer, name, ty, false, options)
    };

    let mut locals: Vec<_> = doc.resolution.locals_visible_at(offset).filter(|(_, l)| matches(&l.name)).collect();
    locals.sort_by_key(|(_, l)| std::cmp::Reverse(l.visible_from));
    for (id, local) in locals {
        if local.name.is_empty() || !seen.insert(local.name.to_string()) {
            continue;
        }
        let ty = infer.local_type_at(id, offset);
        let kind = match local.kind {
            _ if ty.as_fun().is_some() => CompletionItemKind::FUNCTION,
            LocalKind::Param => CompletionItemKind::VARIABLE,
            _ => CompletionItemKind::VARIABLE,
        };
        let mut out = item(&local.name, kind, 0);
        out.detail = detail_of(&local.name, &ty);
        items.push(out);
        items.extend(call_snippet(&local.name, &ty));
    }

    for (file, symbol) in ws.index.visible_globals(doc.file) {
        if !matches(&symbol.name) || !seen.insert(symbol.name.to_string()) {
            continue;
        }
        let is_stub = ws.index.file(file).is_some_and(|f| f.origin == FileOrigin::Stub);
        let mut out = item(&symbol.name, kind_of(symbol.kind, &symbol.ty), if is_stub { 2 } else { 1 });
        out.detail = detail_of(&symbol.name, &symbol.ty);
        out.documentation = symbol.doc.as_ref().map(|d| Documentation::MarkupContent(markdown(d.to_string())));
        if symbol.deprecated {
            out.tags = Some(vec![CompletionItemTag::DEPRECATED]);
        }
        items.push(out);
        items.extend(call_snippet(&symbol.name, &symbol.ty));
    }

    for keyword in KEYWORDS.iter().filter(|k| matches(k)) {
        items.push(item(keyword, CompletionItemKind::KEYWORD, 3));
    }
    if let Some(quote) = snippet_quote {
        for (label, body, description) in SNIPPETS.iter().filter(|(label, ..)| matches(label)) {
            items.push(snippet_item(label, &quoted(body, quote), description));
        }
        if matches("onCache") {
            items.push(on_cache_snippet(infer, "lib.", quote));
        }
    }

    let mut incomplete = false;
    if prefix.len() >= 3 {
        let file_side = ws.index.file(doc.file).and_then(|f| f.side);
        let side = qbx_lua_analysis::side_guard::SideRegions::of(&doc.text, &doc.chunk)
            .effective(offset, file_side)
            .unwrap_or(Side::Shared);
        let mut count = 0;
        for native in natives().filter(|n| matches(n.name) && n.side.is_available_on(side)) {
            if native.name.starts_with("N_0x") || !seen.insert(native.name.to_string()) {
                continue;
            }
            if count == MAX_NATIVES {
                incomplete = true;
                break;
            }
            count += 1;
            let mut out = item(native.name, CompletionItemKind::FUNCTION, 5);
            out.detail = Some(native.signature());
            out.data = Some(json!({ "native": native.name }));
            if native.alias_of.is_some() {
                out.tags = Some(vec![CompletionItemTag::DEPRECATED]);
            }
            items.push(out);
            items.extend(call_snippet(native.name, &Type::Fun(Arc::new(native_fun_type(&native)))));
        }
    } else {
        incomplete = true;
    }
    (items, incomplete)
}

pub fn resolve(mut item: CompletionItem) -> CompletionItem {
    let name = item.data.as_ref().and_then(|d| d.get("native")).and_then(|n| n.as_str()).map(str::to_string);
    if let Some(native) = name.as_deref().and_then(native) {
        let mut text = lua_block(&native.signature());
        text.push_str(&format!("\n\n*{} native* · `{}`", native.side.label(), native.namespace));
        if let Some(docs) = native_docs(native.name) {
            text.push_str("\n\n");
            text.push_str(&docs);
        }
        item.documentation = Some(Documentation::MarkupContent(markdown(text)));
    }
    item
}
