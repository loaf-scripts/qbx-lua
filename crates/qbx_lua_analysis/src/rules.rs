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
pub const EMPTY_BLOCK: &str = "empty-block";
pub const UNBALANCED_ASSIGNMENTS: &str = "unbalanced-assignments";
pub const DUPLICATE_INDEX: &str = "duplicate-index";
pub const DUPLICATE_ARGUMENT: &str = "duplicate-argument";
pub const MISSING_PARAMETER: &str = "missing-parameter";
/// Reported by qbx-lua-ls, which indexes the LuaCATS types; the linter itself has no type information.
pub const UNDEFINED_DOC_NAME: &str = "undefined-doc-name";
/// Reported by qbx-lua-ls, which reads the LuaCATS classes; the linter itself has no type information.
pub const MISSING_FIELDS: &str = "missing-fields";
/// Reported by qbx-lua-ls, which reads the LuaCATS classes; the linter itself has no type information.
pub const ASSIGN_TYPE_MISMATCH: &str = "assign-type-mismatch";
/// Reported by qbx-lua-ls, which reads the LuaCATS classes; the linter itself has no type information.
pub const UNDECLARED_FIELD: &str = "undeclared-field";
/// Reported by qbx-lua-ls, which infers the returned values; the linter itself has no type information.
pub const RETURN_TYPE_MISMATCH: &str = "return-type-mismatch";
/// Reported by qbx-lua-ls, which reads the `@return` annotations with the types they allow.
pub const MISSING_RETURN: &str = "missing-return";
/// Reported by qbx-lua-ls, which reads the LuaCATS types; the linter itself has no type information.
pub const CAST_TYPE_MISMATCH: &str = "cast-type-mismatch";
/// Reported by qbx-lua-ls, which infers the types; the linter itself has no type information.
pub const NO_UNKNOWN: &str = "no-unknown";
pub const CONST_REASSIGN: &str = "const-reassign";
pub const SELF_ASSIGNMENT: &str = "self-assignment";
pub const SELF_COMPARISON: &str = "self-comparison";
pub const COUNT_DOWN_LOOP: &str = "count-down-loop";
/// Reported by qbx-lua-ls, which infers the types; the linter itself has no type information.
pub const IMPOSSIBLE_COMPARISON: &str = "impossible-comparison";
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
    rule(UNDEFINED_FIELD, Correctness, WARN, false, "A field that does not exist is read from a standard library table."),
    rule(UNUSED_LOCAL, Suspicious, WARN, false, "A local variable is never read."),
    rule(UNUSED_FUNCTION, Suspicious, WARN, false, "A local function is never used."),
    rule(UNUSED_ARGUMENT, Style, HINT, false, "A function parameter is never read."),
    rule(UNUSED_LOOP_VARIABLE, Style, HINT, false, "A loop variable is never read."),
    rule(UNUSED_LABEL, Suspicious, WARN, false, "A label is never the target of a goto."),
    rule(UNDEFINED_LABEL, Correctness, ERROR, false, "A goto targets a label that is not visible."),
    rule(REDEFINED_LOCAL, Suspicious, WARN, false, "A local is declared twice in the same scope."),
    rule(SHADOWED_LOCAL, Style, OFF, false, "A local hides a local from an enclosing scope."),
    rule(UNREACHABLE_CODE, Suspicious, WARN, false, "Code follows a return, break or goto and can never run."),
    rule(EMPTY_BLOCK, Style, INFO, false, "A block has no statements."),
    rule(UNBALANCED_ASSIGNMENTS, Suspicious, WARN, false, "An assignment has more values than targets, or leaves targets without a value."),
    rule(DUPLICATE_INDEX, Suspicious, WARN, false, "A table constructor sets the same key twice."),
    rule(DUPLICATE_ARGUMENT, Correctness, ERROR, false, "Two parameters of one function share a name."),
    rule(MISSING_PARAMETER, Correctness, WARN, false, "A function is called without an argument for a parameter its LuaCATS annotations require."),
    rule(UNDEFINED_DOC_NAME, Correctness, WARN, false, "A LuaCATS annotation names a type that no @class, @alias or @enum declares for the file's side (language server only)."),
    rule(MISSING_FIELDS, Correctness, WARN, false, "A table constructor typed as a LuaCATS class leaves out required fields (language server only)."),
    rule(ASSIGN_TYPE_MISMATCH, Correctness, WARN, false, "A table constructor or assignment stores a value of the wrong type in a field of a LuaCATS class, or in a variable typed with @type or @param (language server only)."),
    rule(UNDECLARED_FIELD, Correctness, WARN, false, "A field or key that a strict LuaCATS class does not declare is set or read (language server only)."),
    rule(RETURN_TYPE_MISMATCH, Correctness, WARN, false, "A function returns a value of a different type than its @return annotation declares (language server only)."),
    rule(MISSING_RETURN, Correctness, WARN, false, "A function with a required @return value can end, or return, without it (language server only)."),
    rule(CAST_TYPE_MISMATCH, Correctness, WARN, false, "A ---@cast gives a variable a type that its declared type does not take (language server only)."),
    rule(NO_UNKNOWN, Style, OFF, false, "A parameter, local or loop variable has no type: none is declared and none can be inferred (language server only)."),
    rule(CONST_REASSIGN, Correctness, ERROR, false, "A <const> or <close> local is assigned to."),
    rule(SELF_ASSIGNMENT, Suspicious, WARN, false, "A variable is assigned to itself."),
    rule(SELF_COMPARISON, Suspicious, WARN, false, "Both sides of a comparison are the same expression."),
    rule(COUNT_DOWN_LOOP, Suspicious, WARN, true, "A numeric for loop counts up from a start above its end, or from #list to 1, without a negative step."),
    rule(IMPOSSIBLE_COMPARISON, Suspicious, INFO, false, "Both sides of an == or ~= have types that share no value, so the comparison always gives the same answer (language server only)."),
    rule(LOWERCASE_GLOBAL, Suspicious, WARN, false, "A global with a lowercase first letter is defined; this is usually a missing 'local'."),
    rule(IMPLICIT_GLOBAL, Suspicious, WARN, false, "A global is created from inside a function and never declared at file scope."),
    rule(BUILTIN_OVERWRITE, Suspicious, WARN, false, "A runtime global or native is overwritten."),
    rule(DEPRECATED, Style, WARN, false, "A deprecated runtime function is used."),
    rule(LOOP_NEVER_YIELDS, FiveM, ERROR, false, "An infinite loop has no Wait and will freeze the game or server thread."),
    rule(SOURCE_AFTER_YIELD, FiveM, WARN, false, "The global 'source' is read after a yield or inside a deferred callback, where it may belong to another event."),
    rule(NATIVE_WRONG_SIDE, FiveM, ERROR, false, "A client-only native is called from a server script, or the reverse."),
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
