pub mod arguments;
pub mod assignments;
pub mod assistant;
pub mod callback_payloads;
pub mod casts;
pub mod class_tables;
pub mod code_action;
pub mod comparisons;
pub mod completion;
pub mod definition;
pub mod diagnostics;
pub mod discards;
pub mod doc_names;
pub mod event_call;
pub mod expected;
pub mod folding;
pub mod hover;
pub mod injected_fields;
pub mod inlay;
pub mod member_refs;
mod native_argument;
pub mod nil_checks;
pub mod nui_resource;
pub mod on_type;
pub mod reference;
pub mod references;
pub mod resource_assets;
pub mod resource_details;
pub mod returns;
pub mod semantic_tokens;
pub mod signature;
pub mod strict_classes;
pub mod symbols;
pub mod unknown_types;
pub mod visibility;
pub mod workspace_health;

use crate::document::Document;
use crate::infer::{FileContext, Infer};
use crate::workspace::Workspace;

pub fn with_infer<R>(ws: &Workspace, doc: &Document, f: impl FnOnce(&Infer) -> R) -> R {
    let ctx = FileContext::new(doc.file, &doc.text, &doc.chunk, &doc.resolution);
    let infer = Infer::new(&ctx, &ws.index);
    f(&infer)
}

pub fn markdown(value: String) -> lsp_types::MarkupContent {
    lsp_types::MarkupContent { kind: lsp_types::MarkupKind::Markdown, value }
}

pub fn lua_block(code: &str) -> String {
    format!("```lua\n{code}\n```")
}
