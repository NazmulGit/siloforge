//! Link insertion: module ladder, block-aware native insertion into RAW post content
//! (byte splice, hash-checked), undo, paragraph replacement / sentence append for the LLM
//! modules, post-write verification, LLM prompt builders and LLM output validation.
//!
//! Contract for the inject pipeline: always splice into the freshly fetched `context=edit`
//! raw content, with the `paragraphs.idx` + `paragraphs.hash` recorded at plan time and the
//! same [`SplitMode`] used for chunking. A changed paragraph yields
//! [`InsertError::ContentChanged`] (with `found_at` when the paragraph merely moved).
//!
//! Spec: `docs/research/01-engine-spec.md` §11–§13; bugs B-1, B-4, B-5, B-17, B-18, B-28.

pub mod llm;
pub mod splice;

use serde::{Deserialize, Serialize};
use sf_core::types::InsertModule;

pub use crate::html::SplitMode;
pub use llm::{
    ChatMessage, LLM_MAX_TOKENS, LLM_TEMPERATURE, LlmKind, LlmLimits, LlmRejection, context_prompt,
    rephrase_prompt, strip_code_fences, validate_llm_output,
};
pub use splice::{
    InsertOutcome, InsertRequest, append_to_paragraph, count_links, insert_link, locate_paragraph,
    remove_link, replace_paragraph, urls_match, validate_target_url, verify_link,
};

/// Typed insertion errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum InsertError {
    #[error("content too large")]
    TooLarge,
    #[error("target url must be http(s) or a site-relative path")]
    InvalidUrl,
    #[error("anchor is empty or too long")]
    EmptyAnchor,
    #[error("paragraph {index} not found ({count} paragraphs)")]
    ParagraphNotFound { index: usize, count: usize },
    /// The paragraph at `index` no longer has the expected hash ("content changed").
    #[error("content changed: paragraph {index} hash is {found_hash}")]
    ContentChanged {
        index: usize,
        found_hash: String,
        /// Index where a paragraph with the expected hash now lives, if any.
        found_at: Option<usize>,
    },
    #[error("anchor text not found outside links/headings/code in the paragraph")]
    AnchorNotFound,
    #[error("paragraph already links to the target")]
    AlreadyLinked,
    #[error("link not found")]
    LinkNotFound,
}

const BASE_THRESHOLDS: [(InsertModule, f64); 3] = [
    (InsertModule::Native, 60.0),
    (InsertModule::Rephrase, 40.0),
    (InsertModule::Context, 30.0),
];

/// v1 `pickModule`: preset shift (conservative +10, aggressive −10); first allowed module
/// whose threshold the 0–100 score clears, else `content_gap`. v2 (B-17): `native` is only
/// eligible when the anchor was found in the paragraph (`in_paragraph`).
pub fn pick_module(
    score100: f64,
    allowed: &[InsertModule],
    preset: Option<&str>,
    in_paragraph: bool,
) -> InsertModule {
    let shift = match preset {
        Some("conservative") => 10.0,
        Some("aggressive") => -10.0,
        _ => 0.0,
    };
    let default_allowed = [InsertModule::Native];
    let allowed = if allowed.is_empty() {
        &default_allowed[..]
    } else {
        allowed
    };
    for (module, min) in BASE_THRESHOLDS {
        if !allowed.contains(&module) || (module == InsertModule::Native && !in_paragraph) {
            continue;
        }
        if score100.is_finite() && score100 >= min + shift {
            return module;
        }
    }
    InsertModule::ContentGap
}

#[cfg(test)]
mod tests;
