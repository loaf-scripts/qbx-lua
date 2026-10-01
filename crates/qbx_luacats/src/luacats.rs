use std::sync::Arc;

use qbx_fivem_data::Side;
use smol_str::SmolStr;

use crate::types::{merged_returns, CallbackTag, DescribedValue, FunType, Param, Type, TypeParser};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocParam {
    pub name: SmolStr,
    pub ty: Type,
    pub optional: bool,
    pub description: String,
    /// The values that the `---|` lines under it list, with their descriptions.
    pub values: Vec<DescribedValue>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocReturn {
    pub ty: Type,
    pub name: Option<SmolStr>,
    pub description: String,
    /// The values that the `---|` lines under it list, with their descriptions.
    pub values: Vec<DescribedValue>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocField {
    pub name: SmolStr,
    pub ty: Type,
    pub optional: bool,
    pub description: String,
    /// Index of the doc line the field was declared on, used to locate it in the source.
    pub line: usize,
    /// The side of `@field (server) name type`.
    pub side: Option<Side>,
    /// The values that the `---|` lines under it list, with their descriptions.
    pub values: Vec<DescribedValue>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocIndexField {
    pub key: Type,
    pub ty: Type,
    pub side: Option<Side>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocClass {
    pub name: SmolStr,
    pub parents: Vec<SmolStr>,
    pub fields: Vec<DocField>,
    /// General indices, retained per declaration so each side can select its own.
    pub indices: Vec<DocIndexField>,
    /// `---@field [1] number` and `---@field [true] string`: fields keyed by an integer or boolean
    /// literal, with their values, in declaration order.
    pub literal_fields: Vec<DocIndexField>,
    pub call: Option<Arc<FunType>>,
    pub description: String,
    pub line: usize,
    /// The side of `@class (server) Name`.
    pub side: Option<Side>,
    /// `Some(true)` for `@class (strict) Name` or LuaLS's `(exact)`, `Some(false)` for `(loose)`, and
    /// `None` when the configured default decides.
    pub strict: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocAlias {
    pub name: SmolStr,
    pub ty: Type,
    pub description: String,
    pub line: usize,
    /// The side of `@alias (server) Name`, or of `@enum (server) Name` for the alias an enum becomes.
    pub side: Option<Side>,
    /// The values that the `---|` lines under it list, with their descriptions.
    pub values: Vec<DescribedValue>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocGroup {
    pub description: String,
    pub classes: Vec<DocClass>,
    pub aliases: Vec<DocAlias>,
    pub params: Vec<DocParam>,
    pub returns: Vec<DocReturn>,
    /// The sets of values of `@return false | (string, string)`; `returns` then lists what each
    /// position holds across them.
    pub return_sets: Vec<Vec<Type>>,
    pub ty: Option<Type>,
    /// The types a `@type` line lists after its first, as `---@type string, number` does for the
    /// second name of the statement below.
    pub ty_rest: Vec<Type>,
    /// The values that the `---|` lines under `@type` list for its last type, with their
    /// descriptions.
    pub type_values: Vec<DescribedValue>,
    pub enum_name: Option<SmolStr>,
    pub enum_keys: bool,
    pub enum_side: Option<Side>,
    pub generics: Vec<SmolStr>,
    pub overloads: Vec<Arc<FunType>>,
    pub deprecated: Option<String>,
    pub is_async: bool,
    pub nodiscard: bool,
    pub is_meta: bool,
    pub callback: Option<CallbackTag>,
}

impl DocGroup {
    pub fn has_function_tags(&self) -> bool {
        !self.params.is_empty() || !self.returns.is_empty() || !self.overloads.is_empty()
    }

    /// The `@type` of the name at `index` of the statement below. A line that lists several types
    /// gives each name its own, and one type only the first name, as LuaLS binds it.
    pub fn type_at(&self, index: usize) -> Option<&Type> {
        match index {
            0 => self.ty.as_ref(),
            _ => self.ty_rest.get(index - 1),
        }
    }

    /// The values that `---|` lines list, with their descriptions, for the `@type` of the name at
    /// `index`, as `type_at` binds it. They belong to the last type of the line.
    pub fn type_values_at(&self, index: usize) -> &[DescribedValue] {
        match index == self.ty_rest.len() {
            true => &self.type_values,
            false => &[],
        }
    }

    /// Builds the function type from `@param`/`@return`, keeping the order of the real parameters.
    pub fn fun_type(&self, param_names: &[SmolStr], has_vararg: bool, is_method: bool) -> FunType {
        let mut params: Vec<Param> = param_names
            .iter()
            .map(|name| match self.params.iter().find(|p| p.name == *name) {
                Some(doc) => {
                    Param { name: name.clone(), ty: doc.ty.clone(), optional: doc.optional, values: doc.values.clone() }
                }
                None => Param { name: name.clone(), ty: Type::Unknown, ..Param::default() },
            })
            .collect();
        if has_vararg {
            let ty = self.params.iter().find(|p| p.name == "...").map_or(Type::Any, |p| p.ty.clone());
            params.push(Param { name: "...".into(), ty, ..Param::default() });
        }
        // An overload shares the `@generic` names of its doc comment and is called the same way,
        // unless it lists `self` itself like a `fun(self, ...)` field.
        let overloads = self
            .overloads
            .iter()
            .map(|overload| {
                let lists_self = overload.params.first().is_some_and(|p| p.name == "self");
                let is_method = is_method && !lists_self;
                let lists_receiver = !is_method;
                Arc::new(FunType { is_method, lists_receiver, generics: self.generics.clone(), ..(**overload).clone() })
            })
            .collect();
        let return_values = match self.returns.iter().any(|r| !r.values.is_empty()) {
            true => self.returns.iter().map(|r| r.values.clone()).collect(),
            false => Vec::new(),
        };
        FunType {
            params,
            returns: self.returns.iter().map(|r| r.ty.clone()).collect(),
            return_values,
            return_sets: self.return_sets.clone(),
            returns_inferred: false,
            is_method,
            lists_receiver: !is_method,
            generics: self.generics.clone(),
            overloads,
            side: None,
            callback: self.callback.clone(),
            is_async: self.is_async,
        }
    }

    pub fn param_description(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|p| p.name == name).map(|p| p.description.as_str()).filter(|d| !d.is_empty())
    }
}

fn clean_description(text: &str) -> String {
    let text = text.trim();
    let text = text.strip_prefix('#').or_else(|| text.strip_prefix("--")).unwrap_or(text);
    text.trim().to_string()
}

fn split_tag(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim_start().strip_prefix('@')?;
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some((&rest[..end], rest[end..].trim_start()))
}

/// Removes the `public`, `private`, `protected` and `package` keywords in front of a `@field` name.
fn strip_visibility(mut rest: &str) -> &str {
    for scope in ["public ", "private ", "protected ", "package "] {
        if let Some(stripped) = rest.strip_prefix(scope) {
            rest = stripped.trim_start();
        }
    }
    rest
}

/// The type on a `---| value` line, without its `>` or `+` marker.
fn listed_value(line: &str) -> Option<&str> {
    Some(line.trim_start().strip_prefix('|')?.trim_start_matches(['>', '+', ' ']))
}

/// The declaration that the `---| value` lines after it list more values of.
#[derive(Clone, Copy)]
enum Listing {
    Alias,
    Param,
    Return,
    Type,
    Field,
}

impl Listing {
    /// Adds `value` to the type of the latest such declaration of `group`, which also keeps it with
    /// its `description`.
    fn add(self, group: &mut DocGroup, value: Type, description: String) {
        let target = match self {
            Listing::Alias => group.aliases.last_mut().map(|alias| (&mut alias.ty, &mut alias.values)),
            Listing::Param => group.params.last_mut().map(|param| (&mut param.ty, &mut param.values)),
            Listing::Return => group.returns.last_mut().map(|ret| (&mut ret.ty, &mut ret.values)),
            Listing::Type => group.ty_rest.last_mut().or(group.ty.as_mut()).map(|ty| (ty, &mut group.type_values)),
            Listing::Field => {
                let field = group.classes.last_mut().and_then(|class| class.fields.last_mut());
                field.map(|field| (&mut field.ty, &mut field.values))
            }
        };
        let Some((ty, values)) = target else { return };
        *ty = Type::union([std::mem::take(ty), value.clone()]);
        values.push(DescribedValue { value, description });
        // `---| nil` makes a parameter optional, as `@param name string|nil` does.
        if let (Listing::Param, Some(param)) = (self, group.params.last_mut()) {
            param.optional |= allows_nil(&param.ty);
        }
    }
}

fn allows_nil(ty: &Type) -> bool {
    matches!(ty, Type::Union(types) if types.contains(&Type::Nil))
}

/// Splits attributes such as `(exact)`, `(key)` or `(server)` off the front of a tag. A parenthesized
/// type like the `(fun(): string)` of an `@overload` is no list of words and stays.
fn split_attributes(rest: &str) -> (&str, &str) {
    let Some((attributes, after)) = rest.strip_prefix('(').and_then(|inner| inner.split_once(')')) else {
        return ("", rest);
    };
    let is_word = |word: &str| !word.trim().is_empty() && word.trim().chars().all(|c| c.is_ascii_alphabetic());
    if attributes.split(',').all(is_word) {
        (attributes, after.trim_start())
    } else {
        ("", rest)
    }
}

/// The side named among the attributes: `(server)`, `(client)`, or `(exact, server)`.
fn side_attribute(attributes: &str) -> Option<Side> {
    let named = |side: &str| attributes.split(',').any(|attribute| attribute.trim() == side);
    match (named("client"), named("server")) {
        (true, false) => Some(Side::Client),
        (false, true) => Some(Side::Server),
        _ => None,
    }
}

/// Whether the attributes make a class strict: `(strict)` or `(exact)`, or `(loose)` to opt out.
fn strict_attribute(attributes: &str) -> Option<bool> {
    let named = |names: &[&str]| attributes.split(',').any(|attribute| names.contains(&attribute.trim()));
    if named(&["strict", "exact"]) {
        Some(true)
    } else if named(&["loose"]) {
        Some(false)
    } else {
        None
    }
}

/// Whether a declaration scoped to `declared` applies to code on `side`. Unscoped declarations
/// apply everywhere, and shared code or code of an unknown side sees both sides, the way it sees
/// the globals of both.
pub fn applies_on(declared: Option<Side>, side: Option<Side>) -> bool {
    match (declared, side) {
        (Some(declared), Some(side)) => declared.is_available_on(side),
        _ => true,
    }
}

/// An `@overload` signature, with the side its attributes scope it to.
fn overload(rest: &str) -> Option<Arc<FunType>> {
    let (attributes, rest) = split_attributes(rest);
    match TypeParser::new(rest).parse() {
        Type::Fun(fun) => match side_attribute(attributes) {
            None => Some(fun),
            side => Some(Arc::new(FunType { side, ..(*fun).clone() })),
        },
        _ => None,
    }
}

/// Parses the `---` lines of one contiguous doc comment; every line has its `---` prefix removed.
pub fn parse_doc_lines(lines: &[&str]) -> DocGroup {
    let mut group = DocGroup::default();
    let mut description: Vec<&str> = Vec::new();
    let mut listing: Option<Listing> = None;

    for (index, raw) in lines.iter().enumerate() {
        let line = raw.strip_prefix(' ').unwrap_or(raw);
        if let (Some(value), Some(listing)) = (listed_value(line), listing) {
            let mut parser = TypeParser::new(value);
            let value = parser.parse();
            listing.add(&mut group, value, clean_description(parser.rest()));
            continue;
        }
        let Some((tag, rest)) = split_tag(line) else {
            description.push(line);
            continue;
        };
        listing = None;
        match tag {
            "class" => {
                let (attributes, rest) = split_attributes(rest);
                let (head, parents) = rest.split_once(':').unwrap_or((rest, ""));
                let name = head.split_whitespace().next().unwrap_or("").split('<').next().unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let parents = parents
                    .split(',')
                    .filter_map(|p| p.split_whitespace().next())
                    .map(|p| SmolStr::new(p.split('<').next().unwrap_or(p)))
                    .collect();
                group.classes.push(DocClass {
                    name: SmolStr::new(name),
                    parents,
                    description: description.join("\n").trim().to_string(),
                    line: index,
                    side: side_attribute(attributes),
                    strict: strict_attribute(attributes),
                    ..DocClass::default()
                });
            }
            "field" => {
                if parse_field(rest, index, &mut group) {
                    listing = Some(Listing::Field);
                }
            }
            "overload" if !group.classes.is_empty() && !group.has_function_tags() => {
                if let (Some(fun), Some(class)) = (overload(rest), group.classes.last_mut()) {
                    let owner = Type::Named(class.name.clone(), Vec::new());
                    class.call = Some(if fun.mentions_self() { Arc::new(fun.with_self(&owner)) } else { fun });
                }
            }
            "overload" => group.overloads.extend(overload(rest)),
            "alias" => {
                let (attributes, rest) = split_attributes(rest);
                let mut parser = TypeParser::new(rest);
                let Some(name) = parser.ident() else { continue };
                parser.skip_ws();
                let ty = if parser.rest().trim().is_empty() { Type::Unknown } else { parser.parse() };
                group.aliases.push(DocAlias {
                    name: SmolStr::new(name),
                    ty,
                    description: description.join("\n").trim().to_string(),
                    line: index,
                    side: side_attribute(attributes),
                    values: Vec::new(),
                });
                listing = Some(Listing::Alias);
            }
            "enum" => {
                let (attributes, rest) = split_attributes(rest);
                group.enum_name = rest.split_whitespace().next().map(SmolStr::new);
                group.enum_keys = attributes.split(',').any(|attribute| attribute.trim() == "key");
                group.enum_side = side_attribute(attributes);
            }
            "param" => {
                let mut parser = TypeParser::new(rest);
                let name = if parser.rest().starts_with("...") {
                    parser = TypeParser::new(&rest[3..]);
                    "..."
                } else {
                    match parser.ident() {
                        Some(name) => name,
                        None => continue,
                    }
                };
                let optional = parser.rest().starts_with('?');
                let mut parser = TypeParser::new(parser.rest().trim_start_matches('?'));
                let ty = parser.parse();
                group.params.push(DocParam {
                    name: SmolStr::new(name),
                    optional: optional || allows_nil(&ty),
                    ty,
                    description: clean_description(parser.rest()),
                    values: Vec::new(),
                });
                listing = Some(Listing::Param);
            }
            "return" => {
                let mut parser = TypeParser::new(rest);
                // `@return false | (string, string)` lists the sets of values the function returns.
                // They only hold when the line is the function's one `@return`.
                if let Some(sets) = parser.parse_return_sets() {
                    let description = clean_description(parser.rest());
                    let merged = merged_returns(&sets);
                    if group.returns.is_empty() {
                        group.return_sets = sets;
                    }
                    for (index, ty) in merged.into_iter().enumerate() {
                        let description = if index == 0 { description.clone() } else { String::new() };
                        group.returns.push(DocReturn { ty, name: None, description, values: Vec::new() });
                    }
                    continue;
                }
                group.return_sets.clear();
                // One line may list several values, as in `@return boolean found, string? name`.
                loop {
                    let ty = parser.parse();
                    parser.skip_ws();
                    let remainder = parser.rest();
                    let word = remainder.split(|c: char| !(c.is_alphanumeric() || c == '_')).next().unwrap_or_default();
                    let after = &remainder[word.len()..];
                    let named = !word.is_empty() && (after.is_empty() || after.starts_with([' ', '\t', ',']));
                    let (name, after) = if named { (Some(SmolStr::new(word)), after) } else { (None, remainder) };
                    match after.trim_start().strip_prefix(',') {
                        Some(next) => {
                            group.returns.push(DocReturn { ty, name, ..DocReturn::default() });
                            if next.trim().is_empty() {
                                break;
                            }
                            parser = TypeParser::new(next);
                        }
                        None => {
                            let description = clean_description(after);
                            group.returns.push(DocReturn { ty, name, description, values: Vec::new() });
                            break;
                        }
                    }
                }
                listing = Some(Listing::Return);
            }
            "type" => {
                let mut types = TypeParser::new(rest).parse_list().into_iter();
                group.ty = types.next();
                group.ty_rest = types.collect();
                listing = Some(Listing::Type);
            }
            "generic" => {
                let names = rest.split(',').filter_map(|g| g.trim().split([':', ' ']).next()).filter(|g| !g.is_empty());
                group.generics.extend(names.map(SmolStr::new));
            }
            "vararg" => {
                let ty = TypeParser::new(rest).parse();
                group.params.push(DocParam { name: "...".into(), ty, ..DocParam::default() });
            }
            "deprecated" => group.deprecated = Some(rest.trim().to_string()),
            "async" => group.is_async = true,
            "nodiscard" => group.nodiscard = true,
            "meta" => group.is_meta = true,
            "callback" => group.callback = CallbackTag::parse(rest),
            _ => {}
        }
    }

    group.description = description.join("\n").trim().to_string();
    group
}

/// The text of a `@field` after its `(server)` attributes and `private` keyword, in either order,
/// with the side the attributes name.
fn field_head(rest: &str) -> (Option<Side>, &str) {
    let (before, rest) = split_attributes(rest);
    let (after, rest) = split_attributes(strip_visibility(rest));
    (side_attribute(before).or(side_attribute(after)), rest)
}

/// `ty`, declared for the class `class`, with `self` standing for that class.
fn in_class(class: &DocClass, ty: Type) -> Type {
    match ty.mentions_self() {
        true => ty.with_self(&Type::Named(class.name.clone(), Vec::new())),
        false => ty,
    }
}

/// Reads a `@field` into the latest class of `group`. True when it adds a field of its own, the
/// last of `fields`, which `---| value` lines after it list more values of.
fn parse_field(rest: &str, line: usize, group: &mut DocGroup) -> bool {
    let Some(class) = group.classes.last_mut() else { return false };
    let (side, rest) = field_head(rest);
    if let Some(index) = rest.strip_prefix('[') {
        let mut parser = TypeParser::new(index);
        let key = in_class(class, parser.parse());
        let after = parser.rest().trim_start().strip_prefix(']').unwrap_or(parser.rest());
        let value = in_class(class, TypeParser::new(after).parse());
        match key {
            Type::StringLit(name) => {
                class.fields.push(DocField { name, ty: value, line, side, ..DocField::default() });
                return true;
            }
            key @ (Type::IntLit(_) | Type::BooleanLit(_)) => {
                class.literal_fields.push(DocIndexField { key, ty: value, side });
            }
            key => class.indices.push(DocIndexField { key, ty: value, side }),
        }
        return false;
    }
    let mut parser = TypeParser::new(rest);
    let Some(name) = parser.ident() else { return false };
    let optional = parser.rest().starts_with('?');
    let mut parser = TypeParser::new(parser.rest().trim_start_matches('?'));
    let ty = in_class(class, parser.parse());
    let field = DocField {
        name: SmolStr::new(name),
        ty,
        optional,
        description: clean_description(parser.rest()),
        line,
        side,
        values: Vec::new(),
    };
    let Some(field) = add_signature(&mut class.fields, field) else { return false };
    class.fields.push(field);
    true
}

/// Adds a function field declared again under the name of an unscoped one as another signature of
/// it, the way LuaLS reads a repeated `@field`, scoped to the side of its own line. Gives the field
/// back when it declares something else.
fn add_signature(fields: &mut [DocField], field: DocField) -> Option<DocField> {
    let Some(first) = fields.iter_mut().find(|f| f.name == field.name && f.side.is_none()) else {
        return Some(field);
    };
    let (Type::Fun(fun), Type::Fun(next)) = (&mut first.ty, &field.ty) else { return Some(field) };
    let side = field.side;
    let fun = Arc::make_mut(fun);
    fun.overloads.push(Arc::new(FunType { overloads: Vec::new(), side, ..(**next).clone() }));
    fun.overloads.extend(
        next.overloads
            .iter()
            .map(|overload| Arc::new(FunType { side: overload.side.or(side), ..(**overload).clone() })),
    );
    let described = first.description.split("\n\n").any(|part| part == field.description);
    if !field.description.is_empty() && !described {
        let separator = if first.description.is_empty() { "" } else { "\n\n" };
        first.description = format!("{}{separator}{}", first.description, field.description);
    }
    None
}

fn skip_name(rest: &str) -> Option<&str> {
    let mut parser = TypeParser::new(rest);
    parser.ident()?;
    Some(parser.rest())
}

/// A class or alias name on a doc line.
struct FoundName<'a> {
    /// The byte of the line it starts at.
    start: usize,
    name: &'a str,
    /// The name a `@class`, `@alias` or `@enum` declares, rather than one it refers to.
    declared: bool,
}

/// Collects the class and alias names of one doc line. Every `rest` passed in is a suffix of
/// `line`, so it starts at `line.len() - rest.len()`.
struct TypeNames<'a> {
    line: &'a str,
    found: Vec<FoundName<'a>>,
}

impl<'a> TypeNames<'a> {
    /// Records the name a `@class`, `@alias` or `@enum` declares and returns the text after it.
    fn declared(&mut self, rest: &'a str) -> Option<&'a str> {
        let mut parser = TypeParser::new(rest);
        let name = parser.ident()?;
        self.found.push(FoundName { start: self.line.len() - parser.rest().len() - name.len(), name, declared: true });
        Some(parser.rest())
    }

    /// Records the names in the type `rest` starts with and returns the text after it.
    fn ty(&mut self, rest: &'a str) -> &'a str {
        let start = self.line.len() - rest.len();
        let mut parser = TypeParser::new(rest);
        let names = parser.parse_names().into_iter();
        self.found.extend(names.map(|(offset, name)| FoundName { start: start + offset, name, declared: false }));
        parser.rest()
    }

    fn types(&mut self, rest: &'a str) {
        self.list(rest, |names, rest| Some(names.ty(rest)));
    }

    /// Records the names in the sets of values a `@return` lists, as in `false | (Name, string)`.
    /// Returns whether the line lists such sets.
    fn return_sets(&mut self, rest: &'a str) -> bool {
        let start = self.line.len() - rest.len();
        let Some(names) = TypeParser::new(rest).parse_return_set_names() else { return false };
        let found = names.into_iter().map(|(offset, name)| FoundName { start: start + offset, name, declared: false });
        self.found.extend(found);
        true
    }

    /// Walks a comma separated list, where `item` reads one entry and returns the text after it.
    fn list(&mut self, rest: &'a str, item: impl Fn(&mut Self, &'a str) -> Option<&'a str>) {
        let mut next = Some(rest);
        while let Some(after) = next.and_then(|rest| item(self, rest.trim_start())) {
            next = after.trim_start().strip_prefix(',');
        }
    }
}

/// The class or alias name at byte `offset` of a doc line whose `---` prefix is removed, with the
/// byte it starts at. Parameter, field and return names, literals and built-in types do not count.
pub fn type_name_at(line: &str, offset: usize) -> Option<(usize, &str)> {
    let (_, found) = type_names(line)?;
    found
        .into_iter()
        .find(|found| (found.start..=found.start + found.name.len()).contains(&offset))
        .map(|found| (found.start, found.name))
}

/// The class and alias names a doc line refers to, with the byte each starts at: not the name a
/// `@class`, `@alias` or `@enum` declares, and nothing from `@see`, which may name a function.
pub fn referenced_type_names(line: &str) -> Vec<(usize, &str)> {
    match type_names(line) {
        Some((tag, found)) if tag != "see" => {
            found.into_iter().filter(|found| !found.declared).map(|found| (found.start, found.name)).collect()
        }
        _ => Vec::new(),
    }
}

/// The generic parameters a doc line declares: the `T, K` of `@generic T, K: table` or of
/// `@class Pair<T, K>`.
pub fn declared_generics(line: &str) -> Vec<&str> {
    let Some((tag, rest)) = split_tag(line) else { return Vec::new() };
    let list = match tag {
        "generic" => rest,
        "class" => {
            let head = split_attributes(rest).1.split(':').next().unwrap_or_default();
            match head.split_once('<').and_then(|(_, params)| params.split_once('>')) {
                Some((params, _)) => params,
                None => return Vec::new(),
            }
        }
        _ => return Vec::new(),
    };
    list.split(',').filter_map(|entry| entry.trim().split([':', ' ']).next()).filter(|name| !name.is_empty()).collect()
}

/// What one entry of a `@cast` line does to the type of the variable it names.
#[derive(Clone, Debug, PartialEq)]
pub enum CastEntry {
    /// `T`: the variable holds a `T`.
    Replace(Type),
    /// `+T`, or `+?` for `nil`: it may also hold a `T`.
    Add(Type),
    /// `-T`, or `-?` for `nil`: it holds no `T`.
    Remove(Type),
}

/// A `@cast name T, +T, -?` line: the name it casts, and each entry with the bytes of the line its
/// type is written on.
#[derive(Clone, Debug, PartialEq)]
pub struct DocCast<'a> {
    pub name: &'a str,
    pub entries: Vec<(CastEntry, std::ops::Range<usize>)>,
}

/// Reads a `@cast` doc line whose `---` prefix is removed.
pub fn parse_cast(line: &str) -> Option<DocCast<'_>> {
    let ("cast", rest) = split_tag(line)? else { return None };
    let mut parser = TypeParser::new(rest);
    let name = parser.ident()?;
    let mut entries = Vec::new();
    let mut rest = parser.rest();
    loop {
        let entry = rest.trim_start();
        let (sign, body) = match entry.strip_prefix('+') {
            Some(body) => (Some(true), body),
            None => match entry.strip_prefix('-') {
                Some(body) => (Some(false), body),
                None => (None, entry),
            },
        };
        let (ty, after) = match body.strip_prefix('?').filter(|_| sign.is_some()) {
            Some(after) => (Type::Nil, after),
            None => {
                let mut parser = TypeParser::new(body);
                let ty = parser.parse();
                (ty, parser.rest())
            }
        };
        let written = body[..body.len() - after.len()].trim();
        if written.is_empty() {
            break;
        }
        let start = line.len() - body.trim_start().len();
        entries.push((
            match sign {
                None => CastEntry::Replace(ty),
                Some(true) => CastEntry::Add(ty),
                Some(false) => CastEntry::Remove(ty),
            },
            start..start + written.len(),
        ));
        match after.trim_start().strip_prefix(',') {
            Some(next) => rest = next,
            None => break,
        }
    }
    Some(DocCast { name, entries })
}

/// The name a `@param`, `@field`, `@class`, `@alias` or `@enum` line declares, with the byte it
/// starts at. A field keyed like `[string]` gives its key with the brackets.
pub fn declared_name(line: &str) -> Option<(usize, &str)> {
    let (tag, rest) = split_tag(line)?;
    let rest = match tag {
        "class" | "alias" | "enum" => split_attributes(rest).1,
        "param" if rest.starts_with("...") => return Some((line.len() - rest.len(), "...")),
        "param" => rest,
        "field" => {
            let rest = field_head(rest).1;
            if let Some(key) = rest.strip_prefix('[') {
                let mut parser = TypeParser::new(key);
                parser.parse();
                let after = parser.rest().trim_start().strip_prefix(']')?;
                let start = line.len() - rest.len();
                return Some((start, &line[start..line.len() - after.len()]));
            }
            rest
        }
        _ => return None,
    };
    let mut parser = TypeParser::new(rest);
    let name = parser.ident()?;
    Some((line.len() - parser.rest().len() - name.len(), name))
}

/// The tag of a doc line and the class and alias names on it.
fn type_names(line: &str) -> Option<(&str, Vec<FoundName<'_>>)> {
    let (tag, rest) = match listed_value(line) {
        Some(member) => ("|", member),
        None => split_tag(line)?,
    };
    let mut names = TypeNames { line, found: Vec::new() };
    match tag {
        "class" => {
            let (_, rest) = split_attributes(rest);
            names.declared(rest);
            // Parsing the head as a type skips generic parameters such as the `T` of `Child<T>`.
            let mut head = TypeParser::new(rest);
            head.parse();
            if let Some(parents) = head.rest().trim_start().strip_prefix(':') {
                names.types(parents);
            }
        }
        "alias" => {
            if let Some(rest) = names.declared(split_attributes(rest).1) {
                names.types(rest);
            }
        }
        "enum" => {
            names.declared(split_attributes(rest).1);
        }
        "param" => {
            if let Some(rest) = rest.strip_prefix("...").or_else(|| skip_name(rest)) {
                names.types(rest.trim_start_matches('?'));
            }
        }
        "field" => {
            let (_, rest) = field_head(rest);
            let value = match rest.strip_prefix('[') {
                Some(key) => names.ty(key).trim_start().strip_prefix(']'),
                None => skip_name(rest),
            };
            if let Some(value) = value {
                names.types(value.trim_start_matches('?'));
            }
        }
        "cast" => {
            if let Some(rest) = skip_name(rest) {
                names.list(rest, |names, rest| Some(names.ty(rest.trim_start_matches(['+', '-']))));
            }
        }
        "return" => {
            if !names.return_sets(rest) {
                names.list(rest, |names, rest| {
                    let after = names.ty(rest);
                    Some(skip_name(after).unwrap_or(after))
                });
            }
        }
        "operator" => {
            if let Some(rest) = skip_name(rest).map(str::trim_start) {
                let rest = match rest.strip_prefix('(') {
                    Some(operand) => names.ty(operand).trim_start().strip_prefix(')'),
                    None => Some(rest),
                };
                if let Some(result) = rest.and_then(|rest| rest.trim_start().strip_prefix(':')) {
                    names.ty(result);
                }
            }
        }
        "generic" => names.list(rest, |names, rest| {
            let rest = skip_name(rest)?.trim_start();
            Some(rest.strip_prefix(':').map_or(rest, |constraint| names.ty(constraint)))
        }),
        "see" => {
            names.ty(rest);
        }
        "overload" => names.types(split_attributes(rest).1),
        "type" | "vararg" | "as" | "|" => names.types(rest),
        _ => {}
    }
    Some((tag, names.found))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> DocGroup {
        let lines: Vec<&str> = text.lines().map(|l| l.trim_start().strip_prefix("---").unwrap_or(l)).collect();
        parse_doc_lines(&lines)
    }

    #[test]
    fn function_docs() {
        let doc = parse(
            "---Spawns a vehicle.\n---Second line.\n---@param model string|integer the model\n---@param coords? vector4\n---@param ... any extra\n---@return integer netId # network id\n---@return string? err\n---@deprecated use other\n---@async",
        );
        assert_eq!(doc.description, "Spawns a vehicle.\nSecond line.");
        assert_eq!(doc.params.len(), 3);
        assert_eq!(doc.params[0].description, "the model");
        assert!(doc.params[1].optional);
        assert_eq!(doc.params[2].name, "...");
        assert_eq!(doc.returns[0].name.as_deref(), Some("netId"));
        assert_eq!(doc.returns[0].description, "network id");
        assert_eq!(doc.returns[1].ty.to_string(), "string?");
        assert_eq!(doc.deprecated.as_deref(), Some("use other"));
        assert!(doc.is_async);

        let fun = doc.fun_type(&["model".into(), "coords".into()], true, false);
        assert_eq!(
            fun.signature("spawn"),
            "function spawn(model: string|integer, coords?: vector4, ...: any): integer, string?"
        );
    }

    #[test]
    fn returns_listed_on_one_line() {
        let returns = |text: &str| -> Vec<(String, Option<String>, String)> {
            let returns = parse(text).returns.into_iter();
            returns.map(|r| (r.ty.to_string(), r.name.map(String::from), r.description)).collect()
        };
        let entry = |ty: &str, name: Option<&str>, description: &str| {
            (ty.to_string(), name.map(String::from), description.to_string())
        };
        assert_eq!(returns("---@return boolean, string?"), [entry("boolean", None, ""), entry("string?", None, "")]);
        assert_eq!(
            returns("---@return boolean ok, table<string, integer> counts , string? reason # why it failed"),
            [
                entry("boolean", Some("ok"), ""),
                entry("table<string, integer>", Some("counts"), ""),
                entry("string?", Some("reason"), "why it failed"),
            ]
        );
        // A comma in the description, or one that nothing follows, lists no further value.
        assert_eq!(
            returns("---@return boolean found # true, when it exists"),
            [entry("boolean", Some("found"), "true, when it exists")]
        );
        assert_eq!(returns("---@return boolean,"), [entry("boolean", None, "")]);
        assert_eq!(returns("---@return boolean found."), [entry("boolean", None, "found.")]);
    }

    #[test]
    fn returns_listed_as_sets_of_values() {
        let sets = |doc: &DocGroup| -> Vec<Vec<String>> {
            doc.return_sets.iter().map(|set| set.iter().map(Type::to_string).collect()).collect()
        };
        let merged = |doc: &DocGroup| -> Vec<String> { doc.returns.iter().map(|r| r.ty.to_string()).collect() };

        let doc = parse("---@return false | (string, string) # the names, when found");
        assert_eq!(sets(&doc), [vec!["false"], vec!["string", "string"]]);
        // Each position holds what the sets have there, and `nil` for a set that ends before it.
        assert_eq!(merged(&doc), ["false|string", "string?"]);
        assert_eq!(doc.returns[0].description, "the names, when found");
        assert_eq!(doc.fun_type(&[], false, false).signature("f"), "function f(): false | (string, string)");

        let doc = parse("---@return (Player, integer)|(nil, string)|nil");
        assert_eq!(sets(&doc), [vec!["Player", "integer"], vec!["nil", "string"], vec!["nil"]]);
        assert_eq!(merged(&doc), ["Player?", "integer|string|nil"]);

        // Parentheses without a comma group a type, and further `@return` lines add values, so
        // neither lists sets.
        for text in [
            "---@return (string|number)[]",
            "---@return (fun(): string), boolean",
            "---@return false | (string, string)\n---@return integer",
            "---@return integer\n---@return false | (string, string)",
        ] {
            assert!(parse(text).return_sets.is_empty(), "{text}");
        }
        assert_eq!(merged(&parse("---@return (fun(): string), boolean")), ["fun(): string", "boolean"]);
        assert_eq!(merged(&parse("---@return integer\n---@return false | (string, string)")).len(), 3);

        let ty = crate::types::parse_type("fun(id: integer): false | (string, string)");
        assert_eq!(ty.to_string(), "fun(id: integer): false | (string, string)");
        assert_eq!(ty.as_fun().map(|fun| fun.return_sets.len()), Some(2));
    }

    #[test]
    fn classes_fields_and_aliases() {
        let doc = parse(
            "---A player.\n---@class Player : Entity, Base\n---@field name string the name\n---@field private job? Job\n---@field [string] any\n---@field ['quoted-key'] number\n---@overload fun(id: integer): Player\n---@alias Side\n---| 'client' # runs on the client\n---| 'server'\n---@alias Id integer|string",
        );
        let class = &doc.classes[0];
        assert_eq!(class.name, "Player");
        assert_eq!(class.parents, ["Entity", "Base"]);
        assert_eq!(class.description, "A player.");
        assert_eq!(class.fields.len(), 3);
        assert!(class.fields[1].optional);
        assert_eq!(class.fields[2].name, "quoted-key");
        assert_eq!(class.indices.len(), 1);
        assert!(class.call.is_some());
        assert_eq!(doc.aliases[0].ty.to_string(), "\"client\"|\"server\"");
        assert_eq!(doc.aliases[1].ty.to_string(), "integer|string");
    }

    #[test]
    fn self_in_class_fields_names_the_class() {
        let doc = parse(
            "---@class Node\n---@field parent self?\n---@field children self[]\n---@field [string] fun(self: self): self\n---@overload fun(parent: self): self",
        );
        let class = &doc.classes[0];
        let fields: Vec<String> = class.fields.iter().map(|f| f.ty.to_string()).collect();
        assert_eq!(fields, ["Node?", "Node[]"]);
        assert_eq!(class.indices[0].ty.to_string(), "fun(self: Node): Node");
        assert_eq!(Type::Fun(class.call.clone().unwrap()).to_string(), "fun(parent: Node): Node");

        let doc = parse("---@param other self\n---@return self");
        assert_eq!(doc.params[0].ty.to_string(), "self", "a function's `self` depends on where it is defined");
    }

    #[test]
    fn listed_values_continue_params_returns_types_and_fields() {
        let doc = parse(
            "---Runs.\n---@param mode string the mode\n---| 'fast' # goes quickly\n---|> 'slow'\n---@param level\n---| 1\n---| 2\n---@param key string\n---| nil\n---@return integer, string\n---| 'x'",
        );
        let params: Vec<(String, bool)> = doc.params.iter().map(|p| (p.ty.to_string(), p.optional)).collect();
        assert_eq!(
            params,
            [("string|\"fast\"|\"slow\"".into(), false), ("1|2".into(), false), ("string?".into(), true)]
        );
        assert_eq!(doc.params[0].description, "the mode");
        let returns: Vec<String> = doc.returns.iter().map(|r| r.ty.to_string()).collect();
        assert_eq!(returns, ["integer", "string|\"x\""], "the values belong to the last value of the line");
        assert_eq!(doc.description, "Runs.", "the listed values are no description");

        let types = |doc: DocGroup| -> Vec<String> { doc.ty.iter().chain(&doc.ty_rest).map(Type::to_string).collect() };
        assert_eq!(types(parse("---@type string\n---| 'a'\n---| 'b'")), ["string|\"a\"|\"b\""]);
        assert_eq!(types(parse("---@type integer, string\n---| 'b'")), ["integer", "string|\"b\""]);

        let doc = parse("---@class Opts\n---@field mode string\n---| 'a'\n---@field [string] integer\n---| 'b'");
        assert_eq!(doc.classes[0].fields[0].ty.to_string(), "string|\"a\"");
        assert_eq!(doc.classes[0].indices[0].ty.to_string(), "integer", "an index lists no values");

        // Sets of returned values list theirs in parentheses.
        let doc = parse("---@return false | (string, string)\n---| 'x'");
        assert_eq!(doc.returns.iter().map(|r| r.ty.to_string()).collect::<Vec<_>>(), ["false|string", "string?"]);
    }

    #[test]
    fn listed_values_keep_their_descriptions() {
        let described = |values: &[DescribedValue]| -> Vec<(String, String)> {
            values.iter().map(|v| (v.value.to_string(), v.description.clone())).collect()
        };
        let entry = |value: &str, description: &str| (value.to_string(), description.to_string());
        let doc = parse(
            "---@alias Side 'any'\n---| 'client' # runs on the client\n---| 'server' -- runs on the server\n---@param mode\n---| 'fast' # goes quickly\n---| 'slow'\n---@return boolean ok\n---| nil # when it failed\n---@type string\n---| 'a' # first",
        );
        assert_eq!(
            described(&doc.aliases[0].values),
            [entry("\"client\"", "runs on the client"), entry("\"server\"", "runs on the server")],
            "the type before the lines lists no described values"
        );
        assert_eq!(described(&doc.params[0].values), [entry("\"fast\"", "goes quickly"), entry("\"slow\"", "")]);
        assert_eq!(described(&doc.returns[0].values), [entry("nil", "when it failed")]);
        assert_eq!(described(doc.type_values_at(0)), [entry("\"a\"", "first")]);
        let fun = doc.fun_type(&["mode".into()], false, false);
        assert_eq!(fun.params[0].values, doc.params[0].values, "the parameters of the function keep them");
        assert_eq!(fun.return_values, [doc.returns[0].values.clone()], "and so do its returned values");

        let doc = parse("---@type integer, string\n---| 'b' # second");
        assert!(doc.type_values_at(0).is_empty(), "the values belong to the last type of the line");
        assert_eq!(described(doc.type_values_at(1)), [entry("\"b\"", "second")]);
        let doc = parse("---@class Opts\n---@field mode string\n---| 'a' # first");
        assert_eq!(described(&doc.classes[0].fields[0].values), [entry("\"a\"", "first")]);
    }

    #[test]
    fn literal_keyed_fields() {
        let doc = parse(
            "---@class RawEmployee\n---@field [1] number Source\n---@field [2] string Character name\n---@field [9] boolean Visible\n---@field [true] string\n---@field [integer] any",
        );
        let class = &doc.classes[0];
        let fields: Vec<String> =
            class.literal_fields.iter().map(|field| format!("[{}] {}", field.key, field.ty)).collect();
        assert_eq!(fields, ["[1] number", "[2] string", "[9] boolean", "[true] string"]);
        assert!(class.fields.is_empty());
        let index = class.indices.last().expect("`[integer]` is a general index");
        assert_eq!(format!("[{}] {}", index.key, index.ty), "[integer] any");
    }

    #[test]
    fn class_and_enum_attributes() {
        let doc = parse("---@class (partial) Player : Entity");
        assert_eq!(doc.classes[0].name, "Player");
        assert_eq!(doc.classes[0].parents, ["Entity"]);
        assert_eq!(doc.classes[0].strict, None);
        let strict = |text: &str| parse(text).classes[0].strict;
        assert_eq!(strict("---@class (strict) Test"), Some(true));
        assert_eq!(strict("---@class (server, exact) Test"), Some(true));
        assert_eq!(strict("---@class (loose) Test : Base"), Some(false));
        assert_eq!(strict("---@class Test"), None);
        let doc = parse("---@enum (key) Side");
        assert_eq!(doc.enum_name.as_deref(), Some("Side"));
        assert!(doc.enum_keys);
        assert!(!parse("---@enum Side").enum_keys);
    }

    #[test]
    fn callback_tags() {
        use crate::types::CallbackRole;
        let doc = parse("---@callback register\n---@param name string\n---@param handler fun(source: integer, ...)");
        let fun = doc.fun_type(&["name".into(), "handler".into()], false, false);
        assert_eq!(fun.callback, Some(CallbackTag { role: CallbackRole::Register, family: "".into() }));
        let tag = |text: &str| parse(text).callback;
        assert_eq!(tag("---@callback await shop").unwrap().family, "shop");
        assert_eq!(tag("---@callback trigger # runs the handler").unwrap().family, "");
        assert_eq!(tag("---@callback trigger").unwrap().role, CallbackRole::Trigger);
        assert_eq!(tag("---@callback call"), None);
        assert_eq!(tag("---@callback"), None);
    }

    #[test]
    fn side_attributes() {
        let doc = parse(
            "---@class (exact, server) Account\n---@field (client) hud table\n---@field private (server) bank number\n---@field name string\n---@field (server) ['license-id'] string\n---@overload (client) fun(id: integer): Account",
        );
        let class = &doc.classes[0];
        assert_eq!((class.name.as_str(), class.side), ("Account", Some(Side::Server)));
        let fields: Vec<(&str, Option<Side>)> = class.fields.iter().map(|f| (f.name.as_str(), f.side)).collect();
        assert_eq!(
            fields,
            [
                ("hud", Some(Side::Client)),
                ("bank", Some(Side::Server)),
                ("name", None),
                ("license-id", Some(Side::Server))
            ]
        );
        assert_eq!(class.call.as_ref().unwrap().side, Some(Side::Client));

        let doc = parse("---@alias (client) Key\n---| 'E'\n---| 'F'\n---@alias Id integer");
        assert_eq!(doc.aliases[0].name, "Key");
        assert_eq!(doc.aliases[0].side, Some(Side::Client));
        assert_eq!(doc.aliases[0].ty.to_string(), "\"E\"|\"F\"");
        assert_eq!(doc.aliases[1].side, None);

        let doc = parse("---@enum (key, server) Jobs");
        assert_eq!((doc.enum_name.as_deref(), doc.enum_keys, doc.enum_side), (Some("Jobs"), true, Some(Side::Server)));

        let doc = parse("---@param a string\n---@overload (server) fun(a: string, b: number)\n---@overload fun()\n---@overload (fun(): string)");
        let sides: Vec<Option<Side>> = doc.overloads.iter().map(|o| o.side).collect();
        assert_eq!(sides, [Some(Side::Server), None, None], "a parenthesized type is no attribute list");
        let fun = doc.fun_type(&["a".into()], false, false);
        assert_eq!(fun.overloads[0].side, Some(Side::Server));
        assert_eq!(fun.side, None);

        assert!(applies_on(None, Some(Side::Client)));
        assert!(applies_on(Some(Side::Server), None));
        assert!(applies_on(Some(Side::Server), Some(Side::Shared)));
        assert!(applies_on(Some(Side::Server), Some(Side::Server)));
        assert!(!applies_on(Some(Side::Server), Some(Side::Client)));
    }

    #[test]
    fn repeated_function_fields_become_signatures() {
        let doc = parse(
            "---@class Phone\n---@field Has fun(self: Phone): boolean # client-side\n---@field Has fun(self: Phone, source: number): boolean # server-side\n---@field (server) Has fun(self: Phone, source: number, number: string): boolean\n---@field Count number\n---@field Count string",
        );
        let fields: Vec<&str> = doc.classes[0].fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(fields, ["Has", "Count", "Count"], "only functions gain signatures");
        let has = &doc.classes[0].fields[0];
        assert_eq!(has.description, "client-side\n\nserver-side");
        let fun = has.ty.as_fun().unwrap();
        assert_eq!(fun.params.len(), 1);
        let overloads: Vec<(usize, Option<Side>)> = fun.overloads.iter().map(|o| (o.params.len(), o.side)).collect();
        assert_eq!(overloads, [(2, None), (3, Some(Side::Server))]);

        let doc = parse("---@class Phone\n---@field (client) Has fun(): boolean\n---@field (server) Has fun(source: number): boolean");
        let sides: Vec<Option<Side>> = doc.classes[0].fields.iter().map(|f| f.side).collect();
        assert_eq!(sides, [Some(Side::Client), Some(Side::Server)], "fields scoped to a side stay apart");

        let doc = parse("---@class Phone\n---@field Ring fun() # Rings the phone\n---@field Ring fun(times: number) # the phone\n---@field Ring fun(times: number, loud: boolean) # Rings the phone");
        assert_eq!(doc.classes[0].fields[0].description, "Rings the phone\n\nthe phone", "only repeats are dropped");
    }

    #[test]
    fn bracketed_descriptions_inside_field_types() {
        let doc = parse(
            "---@class Config.Bleeding\n---@field items table<string, {value: number [how much], anim: table {dict: string [dictionary]}}> [items]",
        );
        assert_eq!(doc.classes[0].fields[0].name, "items");
    }

    #[test]
    fn type_and_generics() {
        let doc = parse("---@generic T: table, K\n---@type table<string, fun(): boolean>");
        assert_eq!(doc.generics, ["T", "K"]);
        assert_eq!(doc.type_at(0).unwrap().to_string(), "table<string, fun(): boolean>");
        assert_eq!(doc.type_at(1), None, "one type is the first name's alone");
        assert_eq!(doc.ty.unwrap().to_string(), "table<string, fun(): boolean>");

        let doc = parse("---@type boolean, table<string, number> | string");
        let at = |index| doc.type_at(index).map(Type::to_string);
        assert_eq!(at(0).as_deref(), Some("boolean"));
        assert_eq!(at(1).as_deref(), Some("table<string, number>|string"));
        assert_eq!(at(2), None, "a list types no more names than it has types");
    }

    #[test]
    fn finds_type_names_in_annotations() {
        for line in [
            "@type Gar^age",
            "@type Garage^",
            "@type string, Gar^age",
            "@type (string|Gar^age)[]?",
            "@type table<string, Gar^age>",
            "@type fun(value: Gar^age): boolean",
            "@type fun(): Gar^age",
            "@type async fun(value: Gar^age)",
            "@type { value: Gar^age }",
            "@type { [Gar^age]: boolean }",
            "@type [string, Gar^age]",
            "@param value? Gar^age",
            "@param ... Gar^age",
            "@return string, Gar^age",
            "@return string name, Gar^age value",
            "@return false | (Gar^age, string)",
            "@return (string, integer) | Gar^age",
            "@type fun(): false | (string, Gar^age)",
            "@field private value? Gar^age",
            "@field [Gar^age] string",
            "@field ['value'] Gar^age",
            "@class Gar^age",
            "@class (exact) Child<T>: Parent, Gar^age",
            "@class (partial) Gar^age",
            "@class (partial) Child: Gar^age",
            "@alias Gar^age string",
            "@alias Value Gar^age",
            "@alias (server) Gar^age string",
            "@alias (client) Value Gar^age",
            "@field (server) value? Gar^age",
            "@field private (client) value Gar^age",
            "@overload (server) fun(): Gar^age",
            "| > Gar^age # description",
            "@enum Gar^age",
            "@enum (key) Gar^age",
            "@overload fun(): Gar^age",
            "@operator add(Gar^age): Vector",
            "@operator mul(number): Gar^age",
            "@operator call(): Gar^age",
            "@operator unm: Gar^age",
            "@generic T, U: table<string, Gar^age>",
            "@generic T: string, U: Gar^age",
            "@cast value +string, -Gar^age",
            "@vararg Gar^age",
            "@as Gar^age",
            "@see Gar^age",
            "@see Gar^age for details",
        ] {
            let offset = line.find('^').unwrap();
            let text = line.replace('^', "");
            assert_eq!(type_name_at(&text, offset), Some((text.find("Garage").unwrap(), "Garage")), "{line}");
        }
        let line = "@type Garage.Point[]";
        assert_eq!(type_name_at(line, line.find("Point").unwrap()), Some((6, "Garage.Point")));
    }

    #[test]
    fn lists_the_type_names_a_line_refers_to() {
        fn names(line: &str) -> Vec<&str> {
            referenced_type_names(line).into_iter().map(|(_, name)| name).collect()
        }
        assert_eq!(names("@class Garage : Base, Other"), ["Base", "Other"]);
        assert_eq!(names("@alias Mode Kind|'a'"), ["Kind"]);
        assert_eq!(names("@enum Jobs"), Vec::<&str>::new());
        assert_eq!(names("@param cb fun(point: Point): Garage?"), ["Point", "Garage"]);
        assert_eq!(names("@field (server) spots table<string, Spot>"), ["Spot"]);
        assert_eq!(names("@return string name, integer"), Vec::<&str>::new());
        assert_eq!(names("@see Garage.open"), Vec::<&str>::new(), "@see may name a function");
        assert_eq!(referenced_type_names("@type  Vec"), [(7, "Vec")]);
    }

    #[test]
    fn lists_declared_generic_parameters() {
        assert_eq!(declared_generics("@generic T, K: table, V"), ["T", "K", "V"]);
        assert_eq!(declared_generics("@class (exact) Pair<L, R> : Base"), ["L", "R"]);
        assert!(declared_generics("@class Plain : Base").is_empty());
        assert!(declared_generics("@param value T").is_empty());
    }

    #[test]
    fn reads_cast_lines() {
        let line = "@cast value string?, +integer, -?, +?, - boolean # why";
        let cast = parse_cast(line).unwrap();
        assert_eq!(cast.name, "value");
        let entries: Vec<(CastEntry, &str)> =
            cast.entries.into_iter().map(|(entry, range)| (entry, &line[range])).collect();
        assert_eq!(
            entries,
            [
                (CastEntry::Replace(Type::String.optional()), "string?"),
                (CastEntry::Add(Type::Integer), "integer"),
                (CastEntry::Remove(Type::Nil), "?"),
                (CastEntry::Add(Type::Nil), "?"),
                (CastEntry::Remove(Type::Boolean), "boolean"),
            ]
        );
        let cast = parse_cast("@cast Items +fun(name: string): Item").unwrap();
        assert!(matches!(&cast.entries[..], [(CastEntry::Add(Type::Fun(_)), _)]));
        assert_eq!(parse_cast("@cast value").map(|cast| cast.entries.len()), Some(0));
        assert_eq!(parse_cast("@type value string"), None);
        assert_eq!(parse_cast("@cast"), None);
    }

    #[test]
    fn finds_the_name_a_line_declares() {
        fn name(line: &str) -> Option<&str> {
            declared_name(line).map(|(start, name)| {
                assert_eq!(&line[start..start + name.len()], name, "{line}");
                name
            })
        }
        assert_eq!(name(" @param source number"), Some("source"));
        assert_eq!(name("@param name? string"), Some("name"));
        assert_eq!(name("@param ... any"), Some("..."));
        assert_eq!(name("@field (server) private  money number"), Some("money"));
        assert_eq!(name("@field public (client) hud table"), Some("hud"));
        assert_eq!(name("@field [string] number"), Some("[string]"));
        assert_eq!(name("@field [ 'key' ] boolean"), Some("[ 'key' ]"));
        assert_eq!(name("@class (exact) Pair<L, R> : Base"), Some("Pair"));
        assert_eq!(name("@alias (client) Key 'E'|'F'"), Some("Key"));
        assert_eq!(name("@enum (key) Jobs"), Some("Jobs"));
        assert_eq!(name("@return string name"), None);
        assert_eq!(name("@param"), None);
    }

    #[test]
    fn ignores_non_type_names_in_annotations() {
        for line in [
            "A Gar^age in the description",
            "@pa^ram value Garage",
            "@param Gar^age string",
            "@field Gar^age string",
            "@field ['Gar^age'] string",
            "@field public^ value Garage",
            "@return string Gar^age",
            "@type string # Gar^age",
            "@type string Gar^age",
            "@type 'Gar^age'",
            "@type fun(Gar^age: string): boolean",
            "@type as^ync fun(): string",
            "@type { Gar^age: string }",
            "@type str^ing",
            "@generic Gar^age: string",
            "@class Child<Gar^age>: Parent",
            "@class (Gar^age) Child",
            "@enum (Gar^age) Mode",
            "@alias (Gar^age) Mode string",
            "@field (Gar^age) value string",
            "@cast Gar^age string",
            "@operator Gar^age(number): string",
            "@see string, see Gar^age",
        ] {
            let offset = line.find('^').unwrap();
            assert_eq!(type_name_at(&line.replace('^', ""), offset), None, "{line}");
        }
    }
}
