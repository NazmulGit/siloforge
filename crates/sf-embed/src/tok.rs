//! Tokenizer wrapper (HF `tokenizer.json`), truncation at [`MAX_TOKENS`], no padding (we pad per
//! sub-batch ourselves).

use std::path::Path;

use sf_core::{Error, Result};
use tokenizers::{Tokenizer, TruncationParams};

use crate::{MAX_TOKENS, MAX_TOKENS_LONG, prepare_text};

pub(crate) struct Tok {
    inner: Tokenizer,
    /// Same tokenizer truncating at [`MAX_TOKENS_LONG`].
    long: Tokenizer,
}

impl Tok {
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let mut inner = Tokenizer::from_file(path)
            .map_err(|e| Error::internal(format!("tokenizer {}: {e}", path.display())))?;
        inner.with_padding(None);
        let mut long = inner.clone();
        inner
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| Error::internal(format!("tokenizer truncation: {e}")))?;
        long.with_truncation(Some(TruncationParams {
            max_length: MAX_TOKENS_LONG,
            ..Default::default()
        }))
        .map_err(|e| Error::internal(format!("tokenizer truncation: {e}")))?;
        Ok(Self { inner, long })
    }

    /// Prepare (char cap, empty -> " ") and tokenise; returns token ids per input.
    /// `max_tokens` above [`MAX_TOKENS`] selects the [`MAX_TOKENS_LONG`] tokenizer.
    pub(crate) fn encode(
        &self,
        texts: &[String],
        max_chars: usize,
        max_tokens: usize,
    ) -> Result<Vec<Vec<i64>>> {
        let prepared: Vec<String> = texts.iter().map(|t| prepare_text(t, max_chars)).collect();
        let (tok, cap) = if max_tokens > MAX_TOKENS {
            (&self.long, MAX_TOKENS_LONG)
        } else {
            (&self.inner, MAX_TOKENS)
        };
        let enc = tok
            .encode_batch(prepared, true)
            .map_err(|e| Error::internal(format!("tokenize: {e}")))?;
        Ok(enc
            .iter()
            .map(|e| {
                e.get_ids()
                    .iter()
                    .take(cap)
                    .map(|&x| i64::from(x))
                    .collect()
            })
            .collect())
    }
}
