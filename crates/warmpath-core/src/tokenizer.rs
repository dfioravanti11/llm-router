//! Turning rendered prompt text into token ids.
//!
//! The router's block hashes are only comparable to a worker's cache if both
//! see the same token sequence, so this is the one place where matching the
//! model exactly matters. It is also the place where that match cannot be
//! verified without the model, which is why the seam exists: a deterministic
//! word tokenizer for development against the mock worker, and the real
//! model's tokenizer for anything that talks to vLLM.

use std::collections::HashMap;
use std::path::Path;

/// Anything that can turn prompt text into token ids.
pub trait Tokenizer: Send + Sync {
    fn encode(&self, text: &str) -> Vec<u32>;

    /// Name recorded in run manifests, so a result can never be read without
    /// knowing what produced its token counts.
    fn name(&self) -> &str;
}

/// A deterministic tokenizer that splits on whitespace.
///
/// It is not any model's tokenizer and does not pretend to be. What it gives is
/// the property the block index actually needs during development: identical
/// text produces identical tokens, and a shared prefix of text produces a
/// shared prefix of tokens. Cache-aware routing can be built and measured
/// against the mock worker on that alone.
///
/// Token counts from this differ from a real model's, so latency measured
/// against the mock worker is not a prediction of latency against vLLM. That
/// comparison is R0.5's job.
#[derive(Debug, Default)]
pub struct WordTokenizer;

impl WordTokenizer {
    pub fn new() -> Self {
        Self
    }
}

impl Tokenizer for WordTokenizer {
    fn encode(&self, text: &str) -> Vec<u32> {
        text.split_whitespace().map(hash_word).collect()
    }

    fn name(&self) -> &str {
        "words"
    }
}

/// Map a word to a token id.
///
/// FNV-1a folded to 32 bits. Collisions are possible and harmless: two words
/// colliding makes two different prompts look like a shared prefix, which costs
/// a cache miss on a worker that turned out not to hold the block. No
/// correctness invariant depends on it, which is the same standing rule the
/// block index has.
fn hash_word(word: &str) -> u32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in word.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    (hash ^ (hash >> 32)) as u32
}

/// A tokenizer backed by a fixed vocabulary.
///
/// Used in tests to stand in for a real model's tokenizer without downloading
/// one. Unknown words map to a single unknown id, which is exactly the
/// behaviour that makes a mismatched vocabulary show up as a collapsed hit
/// rate rather than as an error.
#[derive(Debug)]
pub struct VocabTokenizer {
    vocabulary: HashMap<String, u32>,
    unknown: u32,
    name: String,
}

impl VocabTokenizer {
    pub fn new(name: impl Into<String>, words: &[&str]) -> Self {
        let vocabulary = words
            .iter()
            .enumerate()
            .map(|(index, word)| ((*word).to_string(), index as u32 + 1))
            .collect();

        Self {
            vocabulary,
            unknown: 0,
            name: name.into(),
        }
    }
}

impl Tokenizer for VocabTokenizer {
    fn encode(&self, text: &str) -> Vec<u32> {
        text.split_whitespace()
            .map(|word| self.vocabulary.get(word).copied().unwrap_or(self.unknown))
            .collect()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_produces_identical_tokens() {
        let tokenizer = WordTokenizer::new();
        assert_eq!(
            tokenizer.encode("warm the cache"),
            tokenizer.encode("warm the cache")
        );
    }

    #[test]
    fn shared_leading_text_produces_shared_leading_tokens() {
        let tokenizer = WordTokenizer::new();
        let short = tokenizer.encode("a shared system prompt");
        let long = tokenizer.encode("a shared system prompt and then a question");

        assert_eq!(&long[..short.len()], &short[..]);
    }

    #[test]
    fn different_words_produce_different_tokens() {
        let tokenizer = WordTokenizer::new();
        assert_ne!(tokenizer.encode("alpha"), tokenizer.encode("beta"));
    }

    #[test]
    fn whitespace_runs_do_not_change_the_token_sequence() {
        let tokenizer = WordTokenizer::new();
        assert_eq!(
            tokenizer.encode("warm  the\n cache"),
            tokenizer.encode("warm the cache")
        );
    }

    #[test]
    fn empty_text_produces_no_tokens() {
        assert!(WordTokenizer::new().encode("   ").is_empty());
    }

    #[test]
    fn a_vocabulary_maps_known_words_and_folds_the_rest() {
        let tokenizer = VocabTokenizer::new("tiny", &["warm", "the", "cache"]);

        assert_eq!(tokenizer.encode("warm the cache"), vec![1, 2, 3]);
        assert_eq!(tokenizer.encode("warm the fridge"), vec![1, 2, 0]);
        assert_eq!(tokenizer.name(), "tiny");
    }
}

/// The model's own tokenizer, loaded from a Hugging Face `tokenizer.json`.
///
/// This is the one that matters. The router's block boundaries only line up
/// with a worker's if both cut the same token sequence at the same points, and
/// the only way to get the same token sequence is to run the same tokenizer.
/// Everything else in this crate is exact; this is the part that has to be
/// *the model's*.
pub struct HuggingFaceTokenizer {
    inner: tokenizers::Tokenizer,
    name: String,
}

impl std::fmt::Debug for HuggingFaceTokenizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HuggingFaceTokenizer")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl HuggingFaceTokenizer {
    /// Load from a `tokenizer.json` on disk.
    pub fn from_file(path: &Path, name: impl Into<String>) -> Result<Self, TokenizerError> {
        let inner = tokenizers::Tokenizer::from_file(path)
            .map_err(|err| TokenizerError::Load(err.to_string()))?;
        Ok(Self {
            inner,
            name: name.into(),
        })
    }

    pub fn vocabulary_size(&self) -> usize {
        self.inner.get_vocab_size(true)
    }
}

impl Tokenizer for HuggingFaceTokenizer {
    fn encode(&self, text: &str) -> Vec<u32> {
        // Special tokens are *not* added here. The chat template has already
        // written every marker the model expects into the text, so letting the
        // tokenizer add more would produce a sequence the worker never sees.
        match self.inner.encode(text, false) {
            Ok(encoding) => encoding.get_ids().to_vec(),
            Err(error) => {
                // A prompt that will not tokenize costs this request its cache
                // affinity and nothing else. Failing the request instead would
                // turn a routing optimization into an availability risk.
                tracing_encode_failure(&error.to_string());
                Vec::new()
            }
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Kept as a function so the dependency on a logging crate stays in one place.
fn tracing_encode_failure(message: &str) {
    eprintln!("warmpath: tokenizer failed to encode a prompt: {message}");
}

#[derive(Debug, thiserror::Error)]
pub enum TokenizerError {
    #[error("failed to load tokenizer: {0}")]
    Load(String),
}
