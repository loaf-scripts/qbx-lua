use crate::diagnostic::Severity;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Correctness,
    Suspicious,
    Style,
    Performance,
    FiveM,
    Manifest,
    Security,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Correctness => "correctness",
            Category::Suspicious => "suspicious",
            Category::Style => "style",
            Category::Performance => "performance",
            Category::FiveM => "fivem",
            Category::Manifest => "manifest",
            Category::Security => "security",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Rule {
    pub code: &'static str,
    pub category: Category,
    pub default: Option<Severity>,
    pub fixable: bool,
    pub summary: &'static str,
}

const fn rule(
    code: &'static str,
    category: Category,
    default: Option<Severity>,
    fixable: bool,
    summary: &'static str,
) -> Rule {
    Rule { code, category, default, fixable, summary }
}

use Category::*;
const ERROR: Option<Severity> = Some(Severity::Error);
const WARN: Option<Severity> = Some(Severity::Warning);
const INFO: Option<Severity> = Some(Severity::Info);
const HINT: Option<Severity> = Some(Severity::Hint);
const OFF: Option<Severity> = None;

pub const SYNTAX_ERROR: &str = "syntax-error";
pub const UNDEFINED_GLOBAL: &str = "undefined-global";
/// The linter reports it for the standard library tables, and qbx-lua-ls, which reads the LuaCATS
/// types, also for other values.
pub const UNDEFINED_FIELD: &str = "undefined-field";
pub const UNUSED_LOCAL: &str = "unused-local";
pub const UNUSED_FUNCTION: &str = "unused-function";
pub const UNUSED_ARGUMENT: &str = "unused-argument";
pub const UNUSED_LOOP_VARIABLE: &str = "unused-loop-variable";
pub const UNUSED_LABEL: &str = "unused-label";
pub const UNDEFINED_LABEL: &str = "undefined-label";
pub const REDEFINED_LOCAL: &str = "redefined-local";
pub const SHADOWED_LOCAL: &str = "shadowed-local";
pub const UNREACHABLE_CODE: &str = "unreachable-code";
pub const REDUNDANT_RETURN: &str = "redundant-return";
pub const EMPTY_BLOCK: &str = "empty-block";
pub const TRAILING_SPACE: &str = "trailing-space";
pub const UNBALANCED_ASSIGNMENTS: &str = "unbalanced-assignments";
pub const NEWLINE_CALL: &str = "newline-call";
pub const DUPLICATE_INDEX: &str = "duplicate-index";
pub const DUPLICATE_SET_FIELD: &str = "duplicate-set-field";
pub const DUPLICATE_ARGUMENT: &str = "duplicate-argument";
pub const MISSING_PARAMETER: &str = "missing-parameter";
pub const REDUNDANT_PARAMETER: &str = "redundant-parameter";
/// Reported by qbx-lua-ls, which indexes the LuaCATS types; the linter itself has no type information.
pub const UNDEFINED_DOC_NAME: &str = "undefined-doc-name";
pub const UNDEFINED_DOC_PARAM: &str = "undefined-doc-param";
pub const DUPLICATE_DOC_ALIAS: &str = "duplicate-doc-alias";
pub const DUPLICATE_DOC_FIELD: &str = "duplicate-doc-field";
pub const DUPLICATE_DOC_PARAM: &str = "duplicate-doc-param";
pub const DOC_FIELD_NO_CLASS: &str = "doc-field-no-class";
pub const UNKNOWN_OPERATOR: &str = "unknown-operator";
pub const UNKNOWN_CAST_VARIABLE: &str = "unknown-cast-variable";
pub const MISSING_GLOBAL_DOC: &str = "missing-global-doc";
pub const MISSING_LOCAL_EXPORT_DOC: &str = "missing-local-export-doc";
pub const INCOMPLETE_SIGNATURE_DOC: &str = "incomplete-signature-doc";
/// Reported by qbx-lua-ls, which reads the LuaCATS classes; the linter itself has no type information.
pub const MISSING_FIELDS: &str = "missing-fields";
/// Reported by qbx-lua-ls, which reads the LuaCATS classes; the linter itself has no type information.
pub const ASSIGN_TYPE_MISMATCH: &str = "assign-type-mismatch";
/// Reported by qbx-lua-ls, which reads the LuaCATS types; the linter itself has no type information.
pub const PARAM_TYPE_MISMATCH: &str = "param-type-mismatch";
/// Reported by qbx-lua-ls, which reads the LuaCATS classes; the linter itself has no type information.
pub const UNDECLARED_FIELD: &str = "undeclared-field";
/// Reported by qbx-lua-ls, which reads the LuaCATS classes; the linter itself has no type information.
pub const INJECT_FIELD: &str = "inject-field";
/// Reported by qbx-lua-ls, which reads the LuaCATS classes; the linter itself has no type information.
pub const INVISIBLE: &str = "invisible";
/// Reported by qbx-lua-ls, which infers the returned values; the linter itself has no type information.
pub const RETURN_TYPE_MISMATCH: &str = "return-type-mismatch";
/// Reported by qbx-lua-ls, which reads the `@return` annotations with the types they allow.
pub const MISSING_RETURN: &str = "missing-return";
/// Reported by qbx-lua-ls, which reads the `@return` annotations of every function.
pub const REDUNDANT_RETURN_VALUE: &str = "redundant-return-value";
/// Reported by qbx-lua-ls, which reads the `@nodiscard` annotations of every function.
pub const DISCARD_RETURNS: &str = "discard-returns";
/// Reported by qbx-lua-ls, which reads the LuaCATS types; the linter itself has no type information.
pub const CAST_TYPE_MISMATCH: &str = "cast-type-mismatch";
/// Reported by qbx-lua-ls, which infers the types; the linter itself has no type information.
pub const CAST_LOCAL_TYPE: &str = "cast-local-type";
/// Reported by qbx-lua-ls, which infers the types; the linter itself has no type information.
pub const NO_UNKNOWN: &str = "no-unknown";
pub const CONST_REASSIGN: &str = "const-reassign";
pub const SELF_ASSIGNMENT: &str = "self-assignment";
pub const SELF_COMPARISON: &str = "self-comparison";
pub const COUNT_DOWN_LOOP: &str = "count-down-loop";
/// Reported by qbx-lua-ls, which infers the types; the linter itself has no type information.
pub const IMPOSSIBLE_COMPARISON: &str = "impossible-comparison";
/// Reported by qbx-lua-ls, which infers the types; the linter itself has no type information.
pub const NEED_CHECK_NIL: &str = "need-check-nil";
pub const LOWERCASE_GLOBAL: &str = "lowercase-global";
pub const IMPLICIT_GLOBAL: &str = "implicit-global";
pub const BUILTIN_OVERWRITE: &str = "builtin-overwrite";
pub const DEPRECATED: &str = "deprecated";
pub const LOOP_NEVER_YIELDS: &str = "fivem/loop-never-yields";
pub const SOURCE_AFTER_YIELD: &str = "fivem/source-after-yield";
pub const NATIVE_WRONG_SIDE: &str = "fivem/native-wrong-side";
pub const CITIZEN_PREFIX: &str = "fivem/citizen-prefix";
pub const HASH_LITERAL: &str = "fivem/hash-literal";
pub const DEPRECATED_NATIVE_USAGE: &str = "fivem/legacy-native-pattern";
pub const PREFER_CACHE: &str = "qbox/prefer-cache";
pub const LEGACY_CORE_OBJECT: &str = "qbox/legacy-core-object";
pub const IMPORT_NOT_DECLARED: &str = "fivem/import-not-declared";
pub const MANIFEST_MISSING_FIELD: &str = "manifest/missing-field";
pub const MANIFEST_LUA54: &str = "manifest/lua54";
pub const MANIFEST_MISSING_FILE: &str = "manifest/missing-file";
pub const MANIFEST_UNKNOWN_DIRECTIVE: &str = "manifest/unknown-directive";
pub const MANIFEST_UNLISTED_SCRIPT: &str = "manifest/unlisted-script";

pub const MANIFEST_MISSING_DEPENDENCY: &str = "manifest/missing-dependency";
pub const EVENT_ARGUMENT_COUNT: &str = "fivem/event-argument-count";
pub const EVENT_MISSING_ARGUMENTS: &str = "fivem/event-missing-arguments";
pub const EVENT_WRONG_SIDE: &str = "fivem/event-wrong-side";
pub const EXPORT_ARGUMENT_COUNT: &str = "fivem/export-argument-count";
pub const UNKNOWN_EXPORT: &str = "fivem/unknown-export";
pub const RESOURCE_NOT_FOUND: &str = "fivem/resource-not-found";
pub const CLIENT_SUPPLIED_SOURCE: &str = "security/client-supplied-source";
pub const UNVALIDATED_EVENT_ARGUMENT: &str = "security/unvalidated-event-argument";
pub const SQL_CONCATENATION: &str = "security/sql-concatenation";
pub const UNKNOWN_LOCALE_KEY: &str = "qbox/unknown-locale-key";
pub const UNUSED_LOCALE_KEY: &str = "qbox/unused-locale-key";

pub static RULES: &[Rule] = &[
    rule(SYNTAX_ERROR, Correctness, ERROR, false, "The file cannot be parsed by the CfxLua 5.4 runtime."),
    rule(UNDEFINED_GLOBAL, Correctness, WARN, false, "A global is read that no script in the resource, its imports, the runtime or the natives define."),
    rule(UNDEFINED_FIELD, Correctness, WARN, false, "A field that does not exist is read from a standard library table or, in the language server, from a value whose LuaCATS class or table type does not have it."),
    rule(UNUSED_LOCAL, Suspicious, WARN, false, "A local variable is never read."),
    rule(UNUSED_FUNCTION, Suspicious, WARN, false, "A local function is never used."),
    rule(UNUSED_ARGUMENT, Style, HINT, false, "A function parameter is never read."),
    rule(UNUSED_LOOP_VARIABLE, Style, HINT, false, "A loop variable is never read."),
    rule(UNUSED_LABEL, Suspicious, WARN, false, "A label is never the target of a goto."),
    rule(UNDEFINED_LABEL, Correctness, ERROR, false, "A goto targets a label that is not visible."),
    rule(REDEFINED_LOCAL, Suspicious, WARN, false, "A local is declared twice in the same scope."),
    rule(SHADOWED_LOCAL, Style, OFF, false, "A local hides a local from an enclosing scope."),
    rule(UNREACHABLE_CODE, Suspicious, WARN, false, "Code follows a return, break or goto and can never run."),
    rule(REDUNDANT_RETURN, Style, HINT, false, "A function ends with a `return` that gives no values, which changes nothing."),
    rule(EMPTY_BLOCK, Style, INFO, false, "A block has no statements."),
    rule(TRAILING_SPACE, Style, HINT, true, "A line ends in spaces or tabs outside a comment or string."),
    rule(UNBALANCED_ASSIGNMENTS, Suspicious, WARN, false, "An assignment or `local` statement has more values than targets, or leaves targets without a value."),
    rule(NEWLINE_CALL, Suspicious, WARN, false, "A parenthesized expression that starts a line, and is indexed or called, is read as the argument of a call to the expression that ends the line above."),
    rule(DUPLICATE_INDEX, Suspicious, WARN, false, "A table constructor sets the same key twice."),
    rule(DUPLICATE_SET_FIELD, Suspicious, WARN, false, "A function is assigned to the same table field twice in one block of a file, so the second replaces the first."),
    rule(DUPLICATE_ARGUMENT, Correctness, ERROR, false, "Two parameters of one function share a name."),
    rule(MISSING_PARAMETER, Correctness, WARN, false, "A function is called without an argument for a parameter its LuaCATS annotations require."),
    rule(REDUNDANT_PARAMETER, Correctness, WARN, false, "A function is called with more arguments than it has parameters, so the extra values are lost."),
    rule(UNDEFINED_DOC_NAME, Correctness, WARN, false, "A LuaCATS annotation names a type that no @class, @alias or @enum declares for the file's side (language server only)."),
    rule(UNDEFINED_DOC_PARAM, Correctness, WARN, false, "A LuaCATS @param names no parameter of the function its doc comment documents, or no function follows the comment."),
    rule(DUPLICATE_DOC_ALIAS, Suspicious, WARN, false, "An @alias or @enum reuses the name of an alias, enum or class that the same file declares for the same side."),
    rule(DUPLICATE_DOC_FIELD, Suspicious, WARN, false, "A class declares the same field twice in one file for the same side; repeated function fields are overloads."),
    rule(DUPLICATE_DOC_PARAM, Suspicious, WARN, false, "A doc comment has two @param annotations for the same parameter."),
    rule(DOC_FIELD_NO_CLASS, Correctness, WARN, false, "A @field does not directly follow the @class it belongs to, so lua-language-server attaches it to no class."),
    rule(UNKNOWN_OPERATOR, Correctness, WARN, false, "An @operator names an operator that LuaCATS annotations cannot declare, such as `eq`."),
    rule(UNKNOWN_CAST_VARIABLE, Correctness, WARN, false, "A ---@cast names no local that is in scope where it is written."),
    rule(MISSING_GLOBAL_DOC, Style, OFF, false, "A global function has a parameter without @param or returns a value without @return; one that has neither needs a comment."),
    rule(MISSING_LOCAL_EXPORT_DOC, Style, OFF, false, "A local function that a module's returned table or exports() exports, or a function passed to exports(), has a parameter without @param or returns a value without @return; one that has neither needs a comment."),
    rule(INCOMPLETE_SIGNATURE_DOC, Style, OFF, false, "A function whose doc comment has @param or @return annotations leaves out a parameter or a returned value."),
    rule(MISSING_FIELDS, Correctness, WARN, false, "A table constructor typed as a LuaCATS class or shape, or a union of them, leaves out required fields (language server only)."),
    rule(ASSIGN_TYPE_MISMATCH, Correctness, WARN, false, "A table constructor or assignment stores a value of the wrong type in a field of a LuaCATS class, or in a variable typed with @type or @param (language server only)."),
    rule(PARAM_TYPE_MISMATCH, Correctness, WARN, false, "A call passes an argument of a different type than its parameter's LuaCATS annotation declares (language server only)."),
    rule(UNDECLARED_FIELD, Correctness, WARN, false, "A field or key that a strict LuaCATS class does not declare is set or read (language server only)."),
    rule(INJECT_FIELD, Correctness, WARN, false, "A field is set through a value whose LuaCATS class or table type does not have it, rather than through the name that owns the table (language server only)."),
    rule(INVISIBLE, Correctness, WARN, false, "A private, protected or package field or method of a LuaCATS class is used outside its class, subclasses or file (language server only)."),
    rule(RETURN_TYPE_MISMATCH, Correctness, WARN, false, "A function returns a value of a different type than its @return annotation declares (language server only)."),
    rule(MISSING_RETURN, Correctness, WARN, false, "A function with a required @return value can end, or return, without it (language server only)."),
    rule(REDUNDANT_RETURN_VALUE, Correctness, WARN, false, "A function returns more values than its @return annotations declare (language server only)."),
    rule(DISCARD_RETURNS, Correctness, WARN, false, "A call drops the values of a function whose @nodiscard annotation requires using them (language server only)."),
    rule(CAST_TYPE_MISMATCH, Correctness, WARN, false, "A ---@cast gives a variable a type that its declared type does not take (language server only)."),
    rule(CAST_LOCAL_TYPE, Correctness, WARN, false, "A local without @type or @param is given a value that the type of the value it is declared with does not take, such as a string after `local speed = 5` (language server only)."),
    rule(NO_UNKNOWN, Style, OFF, false, "A parameter, local or loop variable has no type, or a value of unknown type goes where a type is declared (language server only)."),
    rule(CONST_REASSIGN, Correctness, ERROR, false, "A <const> or <close> local is assigned to."),
    rule(SELF_ASSIGNMENT, Suspicious, WARN, false, "A variable is assigned to itself."),
    rule(SELF_COMPARISON, Suspicious, WARN, false, "Both sides of a comparison are the same expression."),
    rule(COUNT_DOWN_LOOP, Suspicious, WARN, true, "A numeric for loop counts up from a start above its end, or from #list to 1, without a negative step."),
    rule(IMPOSSIBLE_COMPARISON, Suspicious, INFO, false, "Both sides of an == or ~= have types that share no value, so the comparison always gives the same answer (language server only)."),
    rule(NEED_CHECK_NIL, Correctness, WARN, false, "A local whose declared type allows nil or false is indexed, called, used in arithmetic, concatenation, # or an ordering comparison, or given as a for bound without a check (language server only)."),
    rule(LOWERCASE_GLOBAL, Suspicious, WARN, false, "A global with a lowercase first letter is defined; this is usually a missing 'local'."),
    rule(IMPLICIT_GLOBAL, Suspicious, WARN, false, "A global is created from inside a function and never declared at file scope."),
    rule(BUILTIN_OVERWRITE, Suspicious, WARN, false, "A runtime global or native is overwritten."),
    rule(DEPRECATED, Style, WARN, false, "A deprecated runtime function is used."),
    rule(LOOP_NEVER_YIELDS, FiveM, ERROR, false, "An infinite loop has no Wait and will freeze the game or server thread."),
    rule(SOURCE_AFTER_YIELD, FiveM, WARN, false, "The global 'source' is read after a yield or inside a deferred callback, where it may belong to another event."),
    rule(NATIVE_WRONG_SIDE, FiveM, ERROR, false, "A native or runtime global is used on a side that lacks it, such as a client-only native in a server script, or the server's `os` and `io` libraries in client or unguarded shared code."),
    rule(CITIZEN_PREFIX, FiveM, INFO, true, "Citizen.Wait/CreateThread/SetTimeout have shorter global aliases."),
    rule(HASH_LITERAL, Performance, INFO, true, "GetHashKey/joaat of a string literal can be a compile-time `hash` literal."),
    rule(DEPRECATED_NATIVE_USAGE, Performance, INFO, true, "A slow legacy native pattern has a faster modern replacement."),
    rule(PREFER_CACHE, Performance, HINT, false, "ox_lib's cache already tracks this value; calling the native again is wasted work."),
    rule(LEGACY_CORE_OBJECT, Style, HINT, false, "The QBCore core object is a compatibility bridge; Qbox resources should use exports.qbx_core and modules."),
    rule(IMPORT_NOT_DECLARED, FiveM, WARN, false, "A library global is used but its import is missing from the fxmanifest for this side."),
    rule(MANIFEST_MISSING_FIELD, Manifest, WARN, false, "fxmanifest.lua lacks fx_version or game."),
    rule(MANIFEST_LUA54, Manifest, OFF, true, "fxmanifest.lua does not set lua54 'yes'. Only matters on old server artifacts; current ones always run Lua 5.4."),
    rule(MANIFEST_MISSING_FILE, Manifest, WARN, false, "fxmanifest.lua references a file or glob that matches nothing."),
    rule(MANIFEST_UNKNOWN_DIRECTIVE, Manifest, WARN, false, "A manifest directive looks like a typo of a known one."),
    rule(MANIFEST_UNLISTED_SCRIPT, Manifest, OFF, false, "A Lua file in the resource is not referenced by fxmanifest.lua."),
    rule(MANIFEST_MISSING_DEPENDENCY, Manifest, INFO, false, "Exports of another resource are used but the resource is not listed under dependencies, so load order is not guaranteed."),
    rule(EVENT_ARGUMENT_COUNT, FiveM, WARN, false, "An event is triggered with more arguments than its handler takes; the extra values are lost."),
    rule(EVENT_MISSING_ARGUMENTS, FiveM, INFO, false, "An event is triggered with fewer arguments than its handler declares; fine for optional parameters, a bug otherwise."),
    rule(EVENT_WRONG_SIDE, FiveM, WARN, false, "An event is triggered towards a side where nothing handles it, while a handler exists on the other side."),
    rule(EXPORT_ARGUMENT_COUNT, FiveM, WARN, false, "An export is called with more arguments than the exported function accepts."),
    rule(UNKNOWN_EXPORT, FiveM, INFO, false, "A resource that is part of the workspace does not register the export that is called."),
    rule(RESOURCE_NOT_FOUND, FiveM, INFO, false, "An export of a resource is called outside any condition, but the server's resources folder contains no such resource. The enclosing function may still never run."),
    rule(CLIENT_SUPPLIED_SOURCE, Security, WARN, false, "A server net event takes the player id as an argument; clients can send any id, use the global 'source'."),
    rule(UNVALIDATED_EVENT_ARGUMENT, Security, WARN, false, "A value sent by a client reaches a sensitive call (money, items, commands, code loading) without ever being checked."),
    rule(SQL_CONCATENATION, Security, WARN, false, "A SQL query is built by concatenating or formatting values into it instead of using ? placeholders."),
    rule(UNKNOWN_LOCALE_KEY, Correctness, WARN, false, "locale() is called with a key that the resource's locale file does not define."),
    rule(UNUSED_LOCALE_KEY, Style, INFO, false, "A key of the locale file is never used by the resource's Lua code."),
];

pub fn find(code: &str) -> Option<&'static Rule> {
    RULES.iter().find(|r| r.code == code)
}
