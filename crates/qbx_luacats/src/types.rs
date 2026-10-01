use std::fmt;
use std::sync::Arc;

use qbx_fivem_data::Side;
use smol_str::SmolStr;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Type {
    #[default]
    Unknown,
    Any,
    Nil,
    Boolean,
    Number,
    Integer,
    String,
    Table,
    Function,
    Thread,
    Userdata,
    BooleanLit(bool),
    StringLit(SmolStr),
    IntLit(i64),
    /// A native handle such as `Vehicle` or `Hash`: an integer that keeps its name for display.
    Handle(SmolStr),
    Named(SmolStr, Vec<Type>),
    Array(Box<Type>),
    Map(Box<Type>, Box<Type>),
    Tuple(Vec<Type>),
    Union(Vec<Type>),
    Fun(Arc<FunType>),
    Shape(Arc<Shape>),
    Variadic(Box<Type>),
    /// A global table addressed by its dotted path, e.g. `lib.callback`; members live in the index.
    GlobalTable(SmolStr),
    /// The `exports` object, or one resource of it once indexed (`exports.qbx_core`).
    Exports(Option<SmolStr>),
    /// The value returned by the module a `require` call names; resolved through the index on use.
    Require(SmolStr),
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Param {
    pub name: SmolStr,
    pub ty: Type,
    pub optional: bool,
    /// The values that the `---|` lines under its `@param` list, with their descriptions.
    pub values: Vec<DescribedValue>,
}

/// A value that a `---| value # description` line lists, with its description, which is empty when
/// the line has none.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DescribedValue {
    pub value: Type,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct FunType {
    pub params: Vec<Param>,
    pub returns: Vec<Type>,
    /// The values that `---|` lines list under each `@return`, with their descriptions, or none
    /// when no returned value lists any.
    pub return_values: Vec<Vec<DescribedValue>>,
    /// The sets of values the function returns when it lists more than one, as `(false)` and
    /// `(string, string)` for `@return false | (string, string)`. `returns` then holds what each
    /// position has across the sets.
    pub return_sets: Vec<Vec<Type>>,
    /// `returns` were inferred from the `return`s of the function's body, not declared with `@return`.
    pub returns_inferred: bool,
    pub is_method: bool,
    /// The parameters are those of a function defined with `.`, or of a plain function value, so
    /// they list every value a call passes: a `:` call gives the first one the value before the colon.
    pub lists_receiver: bool,
    /// The names declared with `@generic`, bound from the arguments of each call.
    pub generics: Vec<SmolStr>,
    /// The `@overload` signatures, for calls the declared one does not fit.
    pub overloads: Vec<Arc<FunType>>,
    /// The side an `@overload (server)` or `(client)` signature applies to.
    pub side: Option<Side>,
    /// What a function tagged `@callback` does with the callback names passed to it.
    pub callback: Option<CallbackTag>,
    /// `async fun(...)` or `---@async`: the function may yield, so it runs in a coroutine.
    pub is_async: bool,
}

/// The role of a function tagged `---@callback register|await|trigger [family]`, which wraps a
/// callback system the way `lib.callback.register` and `lib.callback.await` do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackRole {
    /// Registers a handler under a name: `RegisterServerCallback(name, handler)`.
    Register,
    /// Runs the handler of a name and returns its response: `local ok = AwaitServerCallback(name, ...)`.
    Await,
    /// Runs the handler of a name and passes its response on: `TriggerCallback(name, function(ok) end, ...)`.
    Trigger,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackTag {
    pub role: CallbackRole,
    /// Keeps separate callback systems apart; wrappers tagged without one share the empty family.
    pub family: SmolStr,
}

impl CallbackTag {
    /// Reads the text after `@callback`: a role, then an optional family name.
    pub fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let role = match words.next()? {
            "register" => CallbackRole::Register,
            "await" => CallbackRole::Await,
            "trigger" => CallbackRole::Trigger,
            _ => return None,
        };
        let family = words.next().filter(|word| !word.starts_with('#')).unwrap_or_default();
        Some(Self { role, family: SmolStr::new(family) })
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct ShapeField {
    pub name: SmolStr,
    pub ty: Type,
    pub optional: bool,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Shape {
    pub fields: Vec<ShapeField>,
    /// The values of the array part, which `ipairs` visits, apart from the other `[key]` entries.
    pub array: Option<Type>,
    /// The other `[key]: value` entries, as `[string]: integer` and `[integer]: boolean`.
    pub indices: Vec<(Type, Type)>,
}

impl Type {
    pub fn named(name: &str) -> Type {
        match name {
            "any" => Type::Any,
            "nil" | "void" => Type::Nil,
            "boolean" | "bool" => Type::Boolean,
            "number" | "float" => Type::Number,
            "integer" | "int" => Type::Integer,
            "string" => Type::String,
            "table" => Type::Table,
            "function" => Type::Function,
            "thread" => Type::Thread,
            "userdata" | "lightuserdata" => Type::Userdata,
            "true" => Type::BooleanLit(true),
            "false" => Type::BooleanLit(false),
            "unknown" => Type::Unknown,
            _ => Type::Named(SmolStr::new(name), Vec::new()),
        }
    }

    /// How much a type tells us, used to pick the best of several declarations of one name.
    pub fn specificity(&self) -> u8 {
        match self {
            Type::Unknown => 0,
            Type::Any => 1,
            Type::Table | Type::Function | Type::Nil => 2,
            Type::Union(types) => types.iter().map(Type::specificity).max().unwrap_or(0),
            Type::Boolean | Type::Number | Type::Integer | Type::String | Type::Thread | Type::Userdata => 3,
            Type::BooleanLit(_) | Type::StringLit(_) | Type::IntLit(_) => 3,
            _ => 4,
        }
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Type::Unknown)
    }

    pub fn is_literal(&self) -> bool {
        matches!(self, Type::StringLit(_) | Type::IntLit(_) | Type::BooleanLit(_))
    }

    pub fn union(types: impl IntoIterator<Item = Type>) -> Type {
        let mut flat: Vec<Type> = Vec::new();
        for ty in types {
            match ty {
                Type::Union(inner) => inner.into_iter().for_each(|t| push_unique(&mut flat, t)),
                other => push_unique(&mut flat, other),
            }
        }
        if flat.iter().any(|t| matches!(t, Type::Any)) {
            return Type::Any;
        }
        if flat.len() > 1 {
            flat.retain(|t| !t.is_unknown());
        }
        match flat.len() {
            0 => Type::Unknown,
            1 => flat.pop().unwrap_or_default(),
            _ => Type::Union(flat),
        }
    }

    pub fn optional(self) -> Type {
        Type::union([self, Type::Nil])
    }

    pub fn without_nil(&self) -> Type {
        match self {
            Type::Union(types) => Type::union(types.iter().filter(|t| !matches!(t, Type::Nil)).cloned()),
            other => other.clone(),
        }
    }

    /// Widens literal types the way a variable initialised with a literal should be shown.
    pub fn widen(&self) -> Type {
        match self {
            Type::BooleanLit(_) => Type::Boolean,
            Type::StringLit(_) => Type::String,
            Type::IntLit(_) => Type::Integer,
            Type::Union(types) => Type::union(types.iter().map(Type::widen)),
            other => other.clone(),
        }
    }

    /// Widens what a call returns like `widen`, except that a `false` or `true` beside values of
    /// another kind stays: the `false|string` of a function that returns `false` or a name says
    /// more than `boolean|string`.
    pub fn widen_returned(&self) -> Type {
        let Type::Union(types) = self else { return self.widen() };
        let has = |flag: bool| types.contains(&Type::BooleanLit(flag));
        let mixed = types.iter().any(|t| !matches!(t, Type::Nil | Type::Boolean | Type::BooleanLit(_)));
        if !mixed || types.contains(&Type::Boolean) || (has(true) && has(false)) {
            return self.widen();
        }
        Type::union(types.iter().map(|t| if matches!(t, Type::BooleanLit(_)) { t.clone() } else { t.widen() }))
    }

    pub fn as_fun(&self) -> Option<&Arc<FunType>> {
        match self {
            Type::Fun(fun) => Some(fun),
            Type::Union(types) => types.iter().find_map(Type::as_fun),
            _ => None,
        }
    }

    pub fn first_return(&self) -> Type {
        self.as_fun().and_then(|f| f.returns.first().cloned()).unwrap_or_default()
    }

    /// Whether the type names `self`, which a doc comment writes for the class of the table its
    /// function or field belongs to.
    pub fn mentions_self(&self) -> bool {
        match self {
            Type::Named(name, args) => (name == "self" && args.is_empty()) || args.iter().any(Type::mentions_self),
            Type::Array(inner) | Type::Variadic(inner) => inner.mentions_self(),
            Type::Map(key, value) => key.mentions_self() || value.mentions_self(),
            Type::Tuple(types) | Type::Union(types) => types.iter().any(Type::mentions_self),
            Type::Fun(fun) => fun.mentions_self(),
            Type::Shape(shape) => {
                shape.fields.iter().any(|field| field.ty.mentions_self())
                    || shape.array.as_ref().is_some_and(Type::mentions_self)
                    || shape.indices.iter().any(|(key, value)| key.mentions_self() || value.mentions_self())
            }
            _ => false,
        }
    }

    /// The type with each `self` replaced by `owner`, the class it stands for. With no class to
    /// stand for, `self?` is unknown rather than `nil`.
    pub fn with_self(&self, owner: &Type) -> Type {
        match self {
            Type::Named(name, args) if name == "self" && args.is_empty() => owner.clone(),
            Type::Named(name, args) => Type::Named(name.clone(), args.iter().map(|t| t.with_self(owner)).collect()),
            Type::Array(inner) => Type::Array(Box::new(inner.with_self(owner))),
            Type::Variadic(inner) => Type::Variadic(Box::new(inner.with_self(owner))),
            Type::Map(key, value) => Type::Map(Box::new(key.with_self(owner)), Box::new(value.with_self(owner))),
            Type::Tuple(types) => Type::Tuple(types.iter().map(|t| t.with_self(owner)).collect()),
            Type::Union(types) => {
                let parts: Vec<Type> = types.iter().map(|t| t.with_self(owner)).collect();
                if parts.iter().filter(|t| !matches!(t, Type::Nil)).all(Type::is_unknown) {
                    Type::Unknown
                } else {
                    Type::union(parts)
                }
            }
            Type::Fun(fun) => Type::Fun(Arc::new(fun.with_self(owner))),
            Type::Shape(shape) => Type::Shape(Arc::new(Shape {
                fields: shape.fields.iter().map(|f| ShapeField { ty: f.ty.with_self(owner), ..f.clone() }).collect(),
                array: shape.array.as_ref().map(|t| t.with_self(owner)),
                indices: shape
                    .indices
                    .iter()
                    .map(|(key, value)| (key.with_self(owner), value.with_self(owner)))
                    .collect(),
            })),
            other => other.clone(),
        }
    }
}

/// What each position holds across the sets of values a function returns. A set that ends before
/// a position gives `nil` there.
pub fn merged_returns(sets: &[Vec<Type>]) -> Vec<Type> {
    let width = sets.iter().map(Vec::len).max().unwrap_or(0);
    (0..width).map(|i| Type::union(sets.iter().map(|set| set.get(i).cloned().unwrap_or(Type::Nil)))).collect()
}

fn push_unique(list: &mut Vec<Type>, ty: Type) {
    if !list.contains(&ty) {
        list.push(ty);
    }
}

impl FunType {
    /// How a call lines up with `params`, as `(parameters to skip, arguments to skip)`. Functions
    /// declared with `:` do not list `self`, while `fun(self, ...)` fields and functions defined
    /// with `.` list the receiver of a `:` call first.
    pub fn call_offsets(&self, via_colon: bool) -> (usize, usize) {
        let explicit_self = self.lists_receiver || self.params.first().is_some_and(|p| p.name == "self");
        match (via_colon, self.is_method) {
            (true, false) if explicit_self => (1, 0),
            (false, true) => (0, 1),
            _ => (0, 0),
        }
    }

    /// Whether a parameter, returned value or overload names `self`.
    pub fn mentions_self(&self) -> bool {
        self.params.iter().any(|param| param.ty.mentions_self())
            || self.returns.iter().any(Type::mentions_self)
            || self.overloads.iter().any(|overload| overload.mentions_self())
    }

    /// The function with each `self` in its parameters, returned values and overloads replaced by
    /// `owner`.
    pub fn with_self(&self, owner: &Type) -> FunType {
        let types = |types: &[Type]| types.iter().map(|t| t.with_self(owner)).collect();
        FunType {
            params: self.params.iter().map(|p| Param { ty: p.ty.with_self(owner), ..p.clone() }).collect(),
            returns: types(&self.returns),
            return_sets: self.return_sets.iter().map(|set| types(set)).collect(),
            overloads: self.overloads.iter().map(|overload| Arc::new(overload.with_self(owner))).collect(),
            ..self.clone()
        }
    }

    pub fn signature(&self, name: &str) -> String {
        let params: Vec<String> = self.params.iter().map(Param::to_string).collect();
        let mut out = format!("function {name}({})", params.join(", "));
        if !self.returns.is_empty() {
            out.push_str(": ");
            out.push_str(&self.returns_text());
        }
        out
    }

    /// The returned values as written after the `:` of a signature: `integer, string?`, or the
    /// sets of `false | (string, string)`.
    pub fn returns_text(&self) -> String {
        let list = |types: &[Type]| types.iter().map(Type::to_string).collect::<Vec<_>>().join(", ");
        if self.return_sets.is_empty() {
            return list(&self.returns);
        }
        let sets = self.return_sets.iter().map(|set| match set.as_slice() {
            [only] => only.to_string(),
            values => format!("({})", list(values)),
        });
        sets.collect::<Vec<_>>().join(" | ")
    }
}

impl fmt::Display for Param {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let optional = if self.optional { "?" } else { "" };
        if self.ty.is_unknown() {
            write!(f, "{}{optional}", self.name)
        } else {
            write!(f, "{}{optional}: {}", self.name, self.ty)
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Unknown => f.write_str("unknown"),
            Type::Any => f.write_str("any"),
            Type::Nil => f.write_str("nil"),
            Type::Boolean => f.write_str("boolean"),
            Type::Number => f.write_str("number"),
            Type::Integer => f.write_str("integer"),
            Type::String => f.write_str("string"),
            Type::Table => f.write_str("table"),
            Type::Function => f.write_str("function"),
            Type::Thread => f.write_str("thread"),
            Type::Userdata => f.write_str("userdata"),
            Type::BooleanLit(b) => write!(f, "{b}"),
            Type::StringLit(s) => write!(f, "\"{s}\""),
            Type::IntLit(i) => write!(f, "{i}"),
            Type::Handle(name) => f.write_str(name),
            Type::Named(name, args) if args.is_empty() => f.write_str(name),
            Type::Named(name, args) => {
                let args: Vec<String> = args.iter().map(Type::to_string).collect();
                write!(f, "{name}<{}>", args.join(", "))
            }
            Type::Array(inner) => match **inner {
                Type::Union(_) | Type::Fun(_) => write!(f, "({inner})[]"),
                _ => write!(f, "{inner}[]"),
            },
            Type::Map(k, v) => write!(f, "table<{k}, {v}>"),
            Type::Tuple(items) => {
                let items: Vec<String> = items.iter().map(Type::to_string).collect();
                write!(f, "[{}]", items.join(", "))
            }
            Type::Union(types) => {
                let has_nil = types.iter().any(|t| matches!(t, Type::Nil));
                let rest: Vec<String> = types
                    .iter()
                    .filter(|t| !matches!(t, Type::Nil))
                    .filter(|t| !matches!(t, Type::GlobalTable(owner) if owner.starts_with('%')) || types.len() == 1)
                    .map(|t| if matches!(t, Type::Fun(_)) { format!("({t})") } else { t.to_string() })
                    .collect();
                match (has_nil, rest.len()) {
                    (true, 1) => write!(f, "{}?", rest[0]),
                    (true, _) => write!(f, "{}|nil", rest.join("|")),
                    (false, _) => f.write_str(&rest.join("|")),
                }
            }
            Type::Fun(fun) => {
                let params: Vec<String> = fun.params.iter().map(Param::to_string).collect();
                if fun.is_async {
                    f.write_str("async ")?;
                }
                write!(f, "fun({})", params.join(", "))?;
                if !fun.returns.is_empty() {
                    write!(f, ": {}", fun.returns_text())?;
                }
                Ok(())
            }
            Type::Shape(shape) => {
                if shape.fields.is_empty() && shape.array.is_none() && shape.indices.is_empty() {
                    return f.write_str("table");
                }
                let mut parts: Vec<String> = shape
                    .fields
                    .iter()
                    .take(8)
                    .map(|field| format!("{}{}: {}", field.name, if field.optional { "?" } else { "" }, field.ty))
                    .collect();
                if let Some(array) = &shape.array {
                    parts.push(format!("[integer]: {array}"));
                }
                for (k, v) in &shape.indices {
                    parts.push(format!("[{k}]: {v}"));
                }
                if shape.fields.len() > 8 {
                    parts.push("...".into());
                }
                write!(f, "{{ {} }}", parts.join(", "))
            }
            Type::Variadic(inner) => write!(f, "...{inner}"),
            Type::GlobalTable(path) if path.starts_with('%') => f.write_str("table"),
            Type::GlobalTable(path) => f.write_str(path),
            Type::Exports(None) => f.write_str("exports"),
            Type::Exports(Some(resource)) => write!(f, "exports.{resource}"),
            Type::Require(path) => write!(f, "module \"{path}\""),
        }
    }
}

pub struct TypeParser<'a> {
    src: &'a str,
    pos: usize,
    depth: u32,
    /// Class and alias names with their byte offsets, collected only by `parse_names`.
    names: Option<Vec<(usize, &'a str)>>,
}

impl<'a> TypeParser<'a> {
    pub fn new(src: &'a str) -> Self {
        Self { src, pos: 0, depth: 0, names: None }
    }

    /// Parses a type and returns the class and alias names in it with their byte offsets, leaving
    /// out literals, built-in types and the names of fields and parameters.
    pub fn parse_names(&mut self) -> Vec<(usize, &'a str)> {
        self.names = Some(Vec::new());
        self.parse();
        self.names.take().unwrap_or_default()
    }

    pub fn rest(&self) -> &'a str {
        &self.src[self.pos.min(self.src.len())..]
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    fn bytes(&self) -> &'a [u8] {
        self.src.as_bytes()
    }

    fn peek(&self) -> u8 {
        self.bytes().get(self.pos).copied().unwrap_or(0)
    }

    pub fn skip_ws(&mut self) {
        while matches!(self.peek(), b' ' | b'\t') {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.peek() == c {
            self.pos += 1;
            return true;
        }
        false
    }

    pub fn ident(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let start = self.pos;
        while matches!(self.peek(), b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'*')
            && !self.rest().starts_with("...")
        {
            self.pos += 1;
        }
        (self.pos > start).then(|| &self.src[start..self.pos])
    }

    pub fn parse(&mut self) -> Type {
        self.depth += 1;
        if self.depth > 32 {
            self.pos = self.src.len();
            return Type::Unknown;
        }
        let mut types = vec![self.postfix()];
        loop {
            self.skip_ws();
            if self.peek() == b'|' {
                self.pos += 1;
                types.push(self.postfix());
            } else {
                break;
            }
        }
        self.depth -= 1;
        Type::union(types)
    }

    /// The sets of values a function returns, written `false | (string, string)`: alternatives
    /// separated by `|`, where a parenthesized list holds the values of one set. `None`, leaving the
    /// parser where it was, when no alternative lists several values, so the text is a plain type.
    pub fn parse_return_sets(&mut self) -> Option<Vec<Vec<Type>>> {
        let start = (self.pos, self.names.as_ref().map(Vec::len));
        let mut sets = Vec::new();
        loop {
            self.skip_ws();
            sets.push(match self.value_list() {
                Some(values) => values,
                None => vec![self.postfix()],
            });
            self.skip_ws();
            if self.peek() != b'|' {
                break;
            }
            self.pos += 1;
        }
        if sets.iter().all(|set| set.len() == 1) {
            self.rewind(start);
            return None;
        }
        Some(sets)
    }

    /// Like `parse_return_sets`, for the class and alias names in the sets with their byte offsets.
    pub fn parse_return_set_names(&mut self) -> Option<Vec<(usize, &'a str)>> {
        self.names = Some(Vec::new());
        let sets = self.parse_return_sets();
        let names = self.names.take().unwrap_or_default();
        sets.map(|_| names)
    }

    /// The types of a parenthesized list with a comma in it, such as `(string, string)`. A
    /// parenthesized type like `(string|number)` is none and leaves the parser where it was.
    fn value_list(&mut self) -> Option<Vec<Type>> {
        if self.peek() != b'(' {
            return None;
        }
        let start = (self.pos, self.names.as_ref().map(Vec::len));
        self.pos += 1;
        let first = self.parse();
        self.skip_ws();
        if self.peek() != b',' {
            self.rewind(start);
            return None;
        }
        self.pos += 1;
        let mut values = vec![first];
        values.extend(self.list_until(b')'));
        Some(values)
    }

    /// Goes back to a position taken before a parse that did not work out, with the names it found.
    fn rewind(&mut self, (pos, names): (usize, Option<usize>)) {
        self.pos = pos;
        if let (Some(found), Some(len)) = (&mut self.names, names) {
            found.truncate(len);
        }
    }

    /// Comma separated types, as used by `@return` and function returns.
    pub fn parse_list(&mut self) -> Vec<Type> {
        let mut types = vec![self.parse()];
        while self.eat(b',') {
            types.push(self.parse());
        }
        types
    }

    fn postfix(&mut self) -> Type {
        let mut ty = self.primary();
        loop {
            if self.rest().starts_with("[]") {
                self.pos += 2;
                ty = Type::Array(Box::new(ty));
            } else if self.peek() == b'?' {
                self.pos += 1;
                ty = ty.optional();
            } else {
                return ty;
            }
        }
    }

    fn primary(&mut self) -> Type {
        self.skip_ws();
        match self.peek() {
            b'(' => {
                self.pos += 1;
                let inner = self.parse();
                self.eat(b')');
                inner
            }
            b'{' => self.shape(),
            b'[' => {
                self.pos += 1;
                Type::Tuple(self.list_until(b']'))
            }
            quote @ (b'"' | b'\'') => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.src.len() && self.peek() != quote {
                    self.pos += 1;
                }
                let value = SmolStr::new(&self.src[start..self.pos]);
                self.pos = (self.pos + 1).min(self.src.len());
                Type::StringLit(value)
            }
            b'`' => {
                self.pos += 1;
                let name = self.ident().unwrap_or("T");
                self.eat(b'`');
                Type::Named(SmolStr::new(name), Vec::new())
            }
            b'-' | b'0'..=b'9' => {
                let start = self.pos;
                self.pos += 1;
                while self.peek().is_ascii_digit() {
                    self.pos += 1;
                }
                self.src[start..self.pos].parse().map(Type::IntLit).unwrap_or(Type::Integer)
            }
            _ if self.rest().starts_with("...") => {
                self.pos += 3;
                let inner = if self.ident_follows() { self.postfix() } else { Type::Any };
                Type::Variadic(Box::new(inner))
            }
            _ => self.named(),
        }
    }

    fn list_until(&mut self, close: u8) -> Vec<Type> {
        let mut items = Vec::new();
        while !self.eat(close) && self.pos < self.src.len() {
            let before = self.pos;
            let ty = self.parse();
            // A byte no type can start with, like the stray `}` in `table<string, {}}>`, would be retried forever.
            if self.pos == before {
                break;
            }
            items.push(ty);
            self.eat(b',');
        }
        items
    }

    fn ident_follows(&self) -> bool {
        matches!(self.peek(), b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'{' | b'(')
    }

    fn named(&mut self) -> Type {
        let Some(name) = self.ident() else {
            return Type::Unknown;
        };
        // `async fun(...)`, a function that may yield, is the function type marked as async.
        if name == "async" {
            let after = self.pos;
            self.skip_ws();
            if self.rest().starts_with("fun(") {
                return match self.named() {
                    Type::Fun(fun) => Type::Fun(Arc::new(FunType { is_async: true, ..(*fun).clone() })),
                    other => other,
                };
            }
            self.pos = after;
        }
        if name == "fun" && self.peek() == b'(' {
            return self.fun();
        }
        if let Some(names) = &mut self.names {
            if matches!(Type::named(name), Type::Named(..)) {
                names.push((self.pos - name.len(), name));
            }
        }
        if self.peek() != b'<' {
            return Type::named(name);
        }
        self.pos += 1;
        let mut args = self.list_until(b'>');
        match (name, args.len()) {
            ("table", 2) => {
                let value = args.pop().unwrap_or_default();
                let key = args.pop().unwrap_or_default();
                Type::Map(Box::new(key), Box::new(value))
            }
            ("table", 1) => Type::Array(Box::new(args.pop().unwrap_or_default())),
            _ => Type::Named(SmolStr::new(name), args),
        }
    }

    fn fun(&mut self) -> Type {
        self.pos += 1;
        let mut params = Vec::new();
        loop {
            self.skip_ws();
            if self.eat(b')') || self.pos >= self.src.len() {
                break;
            }
            if self.rest().starts_with("...") {
                self.pos += 3;
                let ty = if self.eat(b':') { self.parse() } else { Type::Any };
                params.push(Param { name: "...".into(), ty, ..Param::default() });
            } else if let Some(name) = self.ident() {
                let optional = self.eat(b'?');
                let ty = if self.eat(b':') { self.parse() } else { Type::Unknown };
                params.push(Param { name: SmolStr::new(name), ty, optional, ..Param::default() });
            } else {
                self.pos += 1;
            }
            self.eat(b',');
        }
        let mut return_sets = Vec::new();
        let returns = match self.eat(b':') {
            true => match self.parse_return_sets() {
                Some(sets) => {
                    return_sets = sets;
                    merged_returns(&return_sets)
                }
                None => self.parse_return_list(),
            },
            false => Vec::new(),
        };
        Type::Fun(Arc::new(FunType { params, returns, return_sets, ..FunType::default() }))
    }

    fn parse_return_list(&mut self) -> Vec<Type> {
        let mut types = vec![self.parse()];
        loop {
            let checkpoint = self.pos;
            if !self.eat(b',') {
                break;
            }
            self.skip_ws();
            let before = self.pos;
            let ty = self.parse();
            if self.pos == before || ty.is_unknown() {
                self.pos = checkpoint;
                break;
            }
            types.push(ty);
        }
        types
    }

    fn shape(&mut self) -> Type {
        self.pos += 1;
        let mut shape = Shape::default();
        loop {
            self.skip_ws();
            if self.eat(b'}') || self.pos >= self.src.len() {
                break;
            }
            if self.eat(b'[') {
                let key = self.parse();
                self.eat(b']');
                self.eat(b':');
                let value = self.parse();
                shape.indices.push((key, value));
            } else if let Some(name) = self.ident() {
                let optional = self.eat(b'?');
                let ty = if self.eat(b':') { self.parse() } else { Type::Unknown };
                shape.fields.push(ShapeField { name: SmolStr::new(name), ty, optional });
            } else {
                self.pos += 1;
            }
            if !self.eat(b',') {
                self.eat(b';');
            }
        }
        Type::Shape(Arc::new(shape))
    }
}

pub fn parse_type(text: &str) -> Type {
    TypeParser::new(text).parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(text: &str) -> String {
        parse_type(text).to_string()
    }

    #[test]
    fn parses_common_luacats_types() {
        assert_eq!(roundtrip("string"), "string");
        assert_eq!(roundtrip("string|number"), "string|number");
        assert_eq!(roundtrip("string?"), "string?");
        assert_eq!(roundtrip("number[]"), "number[]");
        assert_eq!(roundtrip("(string|number)[]"), "(string|number)[]");
        assert_eq!(roundtrip("table<string, Player>"), "table<string, Player>");
        assert_eq!(roundtrip("table<Player>"), "Player[]");
        assert_eq!(
            roundtrip("fun(a: string, b?: number): boolean, string"),
            "fun(a: string, b?: number): boolean, string"
        );
        assert_eq!(roundtrip("fun(...: any)"), "fun(...: any)");
        assert_eq!(roundtrip("async fun(x: integer): string"), "async fun(x: integer): string");
        assert_eq!(roundtrip("(async fun())[]"), "(async fun())[]");
        assert_eq!(roundtrip("async"), "async");
        assert_eq!(roundtrip("{ name: string, age?: number }"), "{ name: string, age?: number }");
        assert_eq!(roundtrip("{ [string]: boolean }"), "{ [string]: boolean }");
        assert_eq!(
            roundtrip("{ [string]: integer, [integer]: boolean, name: string }"),
            "{ name: string, [string]: integer, [integer]: boolean }"
        );
        assert_eq!(roundtrip("'left'|'right'"), "\"left\"|\"right\"");
        assert_eq!(roundtrip("[number, number]"), "[number, number]");
        assert_eq!(roundtrip("`T`"), "T");
        assert_eq!(roundtrip("vector3|vector4"), "vector3|vector4");
        assert_eq!(roundtrip("OxPlayer?"), "OxPlayer?");
        assert_eq!(roundtrip("1|2|3"), "1|2|3");
        assert_eq!(roundtrip("any|string"), "any");
    }

    #[test]
    fn stops_at_the_description() {
        let mut parser = TypeParser::new("string|number the value to use");
        assert_eq!(parser.parse().to_string(), "string|number");
        assert_eq!(parser.rest().trim(), "the value to use");
    }

    #[test]
    fn malformed_types_do_not_hang() {
        for text in [
            "fun(",
            "{ a: ",
            "table<",
            "[",
            "((((",
            "fun(a: fun(b: fun(",
            "|||",
            "",
            "table<}>",
            "[}]",
            "table<string, {}}>",
        ] {
            let _ = parse_type(text);
        }
    }

    #[test]
    fn widening_and_nil_removal() {
        assert_eq!(parse_type("'a'|1|true").widen().to_string(), "string|integer|boolean");
        assert_eq!(parse_type("string?").without_nil().to_string(), "string");
    }
}
