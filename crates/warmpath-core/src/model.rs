//! Loading a real model's tokenizer and chat template.
//!
//! # Why this is the highest-risk part of the project
//!
//! The router decides where a request goes by comparing its prompt's block
//! hashes against what it believes each worker holds. Those hashes are the
//! router's own and never leave it, so they do not need to equal anything
//! vLLM computes. What *must* be equal is the thing underneath them: the token
//! sequence, and where the block boundaries fall in it.
//!
//! If the router tokenizes differently from the worker, or renders the
//! conversation differently, then two requests the worker considers to share a
//! prefix may not share one here, or worse, the reverse. Nothing errors. The
//! hit rate simply comes out mediocre, which reads as "cache-aware routing does
//! not help much" rather than as a bug. That failure mode is why the spec calls
//! this the highest-risk item, and it is why the fix is to run the model's own
//! tokenizer and the model's own chat template rather than approximations of
//! them.
//!
//! The remaining half of the check needs hardware: comparing the router's
//! predicted hit rate against vLLM's `prefix_cache_queries` and
//! `prefix_cache_hits`. That is R0.5.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::template::ChatTemplate;
use crate::tokenizer::{HuggingFaceTokenizer, TokenizerError};
use crate::PromptBuilder;

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model directory {0} does not contain {1}")]
    Missing(PathBuf, &'static str),
    #[error("failed to read {0}: {1}")]
    Read(PathBuf, String),
    #[error("failed to parse {0}: {1}")]
    Parse(PathBuf, String),
    #[error("{0} has no chat_template; this model cannot render a conversation")]
    NoChatTemplate(PathBuf),
    #[error(transparent)]
    Tokenizer(#[from] TokenizerError),
    #[error(transparent)]
    Template(#[from] crate::TemplateError),
}

/// The subset of `tokenizer_config.json` the router reads.
#[derive(Debug, Deserialize)]
struct TokenizerConfig {
    #[serde(default)]
    chat_template: Option<String>,
}

/// A model directory holding `tokenizer.json` and `tokenizer_config.json`, as
/// downloaded from the Hugging Face hub.
#[derive(Debug, Clone)]
pub struct ModelFiles {
    pub directory: PathBuf,
}

impl ModelFiles {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    pub fn tokenizer_path(&self) -> PathBuf {
        self.directory.join("tokenizer.json")
    }

    pub fn tokenizer_config_path(&self) -> PathBuf {
        self.directory.join("tokenizer_config.json")
    }

    pub fn is_present(&self) -> bool {
        self.tokenizer_path().is_file() && self.tokenizer_config_path().is_file()
    }

    /// Build a prompt builder that renders and tokenizes the way this model
    /// does.
    pub fn load(&self, block_size: usize) -> Result<PromptBuilder, ModelError> {
        let tokenizer_path = self.tokenizer_path();
        if !tokenizer_path.is_file() {
            return Err(ModelError::Missing(
                self.directory.clone(),
                "tokenizer.json",
            ));
        }
        let config_path = self.tokenizer_config_path();
        if !config_path.is_file() {
            return Err(ModelError::Missing(
                self.directory.clone(),
                "tokenizer_config.json",
            ));
        }

        let name = self
            .directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "model".to_string());
        let tokenizer = HuggingFaceTokenizer::from_file(&tokenizer_path, name)?;

        let text = std::fs::read_to_string(&config_path)
            .map_err(|err| ModelError::Read(config_path.clone(), err.to_string()))?;
        let config: TokenizerConfig = serde_json::from_str(&text)
            .map_err(|err| ModelError::Parse(config_path.clone(), err.to_string()))?;
        let source = config
            .chat_template
            .ok_or_else(|| ModelError::NoChatTemplate(config_path.clone()))?;

        Ok(PromptBuilder::new(
            ChatTemplate::jinja(&source)?,
            Box::new(tokenizer),
            block_size,
        ))
    }
}

/// Where the tests and the local stack look for a downloaded model.
pub fn default_model_directory() -> PathBuf {
    Path::new(".cache").join("qwen3-1.7b")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{shared_prefix_len, Message};

    /// The real model, when it has been downloaded.
    ///
    /// These tests are the fidelity check, so they must not be quietly skipped
    /// in a way nobody notices. `make fetch-model` downloads the files, and the
    /// skip message says so.
    fn model() -> Option<PromptBuilder> {
        // Tests run with the crate root as the working directory.
        let directory = Path::new("../..").join(default_model_directory());
        let files = ModelFiles::new(directory);
        if !files.is_present() {
            eprintln!(
                "skipping model fidelity test: no model at {}. Run `make fetch-model`.",
                files.directory.display()
            );
            return None;
        }
        Some(files.load(16).expect("the model files should load"))
    }

    fn conversation() -> Vec<Message> {
        vec![
            Message::new("system", "You are a helpful assistant."),
            Message::new("user", "What is prefix caching?"),
            Message::new("assistant", "It reuses KV state for repeated prefixes."),
            Message::new("user", "How does a router exploit it?"),
        ]
    }

    #[test]
    fn the_real_template_renders_the_whole_conversation() {
        let Some(builder) = model() else { return };

        let fingerprint = builder
            .fingerprint_chat(&conversation())
            .expect("should fingerprint");

        assert!(fingerprint.token_count > 30, "{fingerprint:?}");
    }

    /// The bug this whole module exists to prevent, checked against the real
    /// template rather than a stand-in.
    #[test]
    fn a_real_conversation_does_not_fingerprint_like_its_first_message() {
        let Some(builder) = model() else { return };

        let full = builder
            .fingerprint_chat(&conversation())
            .expect("should fingerprint");
        let first_only = builder
            .fingerprint_chat(&conversation()[..1])
            .expect("should fingerprint");

        assert!(full.token_count > first_only.token_count * 2);
    }

    /// The property multi-turn cache reuse depends on, under the real
    /// tokenizer: adding a turn must extend the block chain, not rewrite it.
    #[test]
    fn a_later_turn_extends_the_real_block_chain() {
        let Some(builder) = model() else { return };

        let long_system = Message::new("system", "You are a helpful assistant. ".repeat(40));
        let first_turn = vec![long_system.clone(), Message::new("user", "First question.")];
        let mut second_turn = first_turn.clone();
        second_turn.push(Message::new("assistant", "First answer."));
        second_turn.push(Message::new("user", "Second question."));

        let first = builder
            .fingerprint_chat(&first_turn)
            .expect("should fingerprint");
        let second = builder
            .fingerprint_chat(&second_turn)
            .expect("should fingerprint");

        assert!(first.block_count() > 4, "{first:?}");
        let shared = shared_prefix_len(&first.blocks, &second.blocks);
        assert!(
            shared >= first.block_count() - 1,
            "only {shared} of {} blocks survived the next turn",
            first.block_count()
        );
    }

    #[test]
    fn conversations_sharing_a_real_system_prompt_share_leading_blocks() {
        let Some(builder) = model() else { return };

        let system = Message::new("system", "You are a helpful assistant. ".repeat(40));
        let left = builder
            .fingerprint_chat(&[
                system.clone(),
                Message::new("user", "Tell me about routers. ".repeat(10)),
            ])
            .expect("should fingerprint");
        let right = builder
            .fingerprint_chat(&[
                system,
                Message::new("user", "Tell me about caches. ".repeat(10)),
            ])
            .expect("should fingerprint");

        let shared = shared_prefix_len(&left.blocks, &right.blocks);
        assert!(shared >= 4, "only {shared} blocks shared");
        assert_ne!(left.blocks, right.blocks, "the questions differ");
    }

    #[test]
    fn the_real_tokenizer_is_the_models_own() {
        let Some(builder) = model() else { return };

        // Qwen3's vocabulary is far larger than any stand-in would produce, so
        // this catches a builder that silently fell back to the word
        // tokenizer.
        assert_eq!(builder.tokenizer_name(), "qwen3-1.7b");
        assert_eq!(builder.block_size(), 16);
    }

    #[test]
    fn a_missing_model_directory_is_an_error_rather_than_a_fallback() {
        let files = ModelFiles::new("does/not/exist");
        assert!(!files.is_present());

        let error = files.load(16).expect_err("should refuse to load");
        assert!(
            matches!(error, ModelError::Missing(_, "tokenizer.json")),
            "{error:?}"
        );
    }
}
