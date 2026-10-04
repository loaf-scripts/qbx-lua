//! `unknown-diag-code`: a `---@diagnostic` or `-- qbx-lint:` comment that names a code no rule here
//! has and lua-language-server does not know either, so it suppresses nothing. The codes of
//! lua-language-server count even when no rule here checks the same, so comments written for it are
//! left alone.

use super::{FileInput, Sink};
use crate::directives::directive_codes;
use crate::lua_ls_config::{is_lua_ls_code, LUA_LS_CODES};
use crate::manifest::edit_distance;
use crate::rules;

pub(super) fn check(input: &FileInput, sink: &mut Sink) {
    if sink.enabled(rules::UNKNOWN_DIAG_CODE).is_none() {
        return;
    }
    for (span, code) in directive_codes(input.source, &input.chunk.comments) {
        if rules::find(code).is_some() || is_lua_ls_code(code) {
            continue;
        }
        let message = match closest_code(code) {
            Some(known) => format!("unknown diagnostic code '{code}'; did you mean '{known}'?"),
            None => format!("unknown diagnostic code '{code}'"),
        };
        sink.report(rules::UNKNOWN_DIAG_CODE, span, message);
    }
}

/// The rule or lua-language-server code that `code` most likely misspells.
fn closest_code(code: &str) -> Option<&'static str> {
    let known = rules::RULES.iter().map(|rule| rule.code).chain(LUA_LS_CODES.iter().copied());
    known
        .map(|known| (known, edit_distance(code, known)))
        .filter(|(_, distance)| *distance <= 2)
        .min_by_key(|(_, distance)| *distance)
        .map(|(known, _)| known)
}
