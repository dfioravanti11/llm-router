//! Prompt rendering, tokenization, and block hashing.
//!
//! Shared by the router and the mock worker so that each computes block hashes
//! from the request body independently, using the same rules. That
//! independence is deliberate. If the router's hashing drifts from what a
//! worker would compute, the symptom is a collapsed cache hit rate rather than
//! a silent agreement between two copies of the same mistake.

pub mod blocks;
pub mod model;
pub mod template;
pub mod tokenizer;

use serde::{Deserialize, Serialize};

pub use blocks::{hash_chain, shared_prefix_len, BlockHash, DEFAULT_BLOCK_SIZE};
pub use model::{ModelError, ModelFiles};
pub use template::{ChatTemplate, Message, TemplateError};
pub use tokenizer::{HuggingFaceTokenizer, Tokenizer, VocabTokenizer, WordTokenizer};

/// What the router needs to know about a request in order to route it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFingerprint {
    /// Chained block hashes, oldest first.
    pub blocks: Vec<BlockHash>,
    /// Tokens in the rendered prompt, whole blocks and the trailing partial one.
    pub token_count: usize,
}

impl PromptFingerprint {
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Renders a conversation, tokenizes it, and hashes the block chain.
pub struct PromptBuilder {
    template: ChatTemplate,
    tokenizer: Box<dyn Tokenizer>,
    block_size: usize,
}

impl std::fmt::Debug for PromptBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptBuilder")
            .field("tokenizer", &self.tokenizer.name())
            .field("block_size", &self.block_size)
            .finish_non_exhaustive()
    }
}

impl PromptBuilder {
    pub fn new(template: ChatTemplate, tokenizer: Box<dyn Tokenizer>, block_size: usize) -> Self {
        assert!(block_size > 0, "block size must be positive");
        Self {
            template,
            tokenizer,
            block_size,
        }
    }

    /// A builder suitable for development against the mock worker.
    pub fn simple(block_size: usize) -> Self {
        Self::new(
            ChatTemplate::Simple,
            Box::new(WordTokenizer::new()),
            block_size,
        )
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn tokenizer_name(&self) -> &str {
        self.tokenizer.name()
    }

    /// Fingerprint a chat conversation.
    ///
    /// The whole conversation is rendered, never just the newest message.
    pub fn fingerprint_chat(
        &self,
        messages: &[Message],
    ) -> Result<PromptFingerprint, TemplateError> {
        let rendered = self.template.render(messages, true)?;
        Ok(self.fingerprint_text(&rendered))
    }

    /// Fingerprint prompt text that is already rendered, as the legacy
    /// completions endpoint sends.
    pub fn fingerprint_text(&self, text: &str) -> PromptFingerprint {
        let token_ids = self.tokenizer.encode(text);
        PromptFingerprint {
            blocks: hash_chain(&token_ids, self.block_size),
            token_count: token_ids.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(count: usize) -> String {
        (0..count)
            .map(|index| format!("w{index}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn a_longer_conversation_extends_the_block_chain_of_a_shorter_one() {
        let builder = PromptBuilder::simple(16);

        let first_turn = vec![
            Message::new("system", words(64)),
            Message::new("user", words(32)),
        ];
        let mut second_turn = first_turn.clone();
        second_turn.push(Message::new("assistant", words(32)));
        second_turn.push(Message::new("user", words(16)));

        let first = builder
            .fingerprint_chat(&first_turn)
            .expect("should fingerprint");
        let second = builder
            .fingerprint_chat(&second_turn)
            .expect("should fingerprint");

        assert!(first.block_count() > 3);
        assert!(second.block_count() > first.block_count());

        // Turn two keeps turn one's blocks, minus at most the block holding the
        // generation prompt, which turn two overwrote with more conversation.
        let shared = shared_prefix_len(&first.blocks, &second.blocks);
        assert!(
            shared >= first.block_count() - 1,
            "only {shared} of {} blocks survived the next turn",
            first.block_count()
        );
    }

    #[test]
    fn conversations_sharing_a_system_prompt_share_leading_blocks() {
        let builder = PromptBuilder::simple(16);
        let system = Message::new("system", words(128));

        // The questions have to be long enough to fill blocks of their own.
        // A difference confined to the trailing partial block would not reach
        // the chain at all, which the next test covers.
        let left = builder
            .fingerprint_chat(&[
                system.clone(),
                Message::new("user", format!("left {}", words(64))),
            ])
            .expect("should fingerprint");
        let right = builder
            .fingerprint_chat(&[system, Message::new("user", format!("right {}", words(64)))])
            .expect("should fingerprint");

        let shared = shared_prefix_len(&left.blocks, &right.blocks);
        assert!(
            shared >= 7,
            "a 128 word shared system prompt yielded only {shared} shared blocks"
        );
        assert!(
            left.block_count() > shared,
            "the questions should have diverged somewhere"
        );
        assert_ne!(left.blocks, right.blocks);
    }

    /// Two prompts that differ only inside the trailing partial block hash
    /// identically, and that is correct rather than a collision.
    ///
    /// A worker cannot serve a cache hit from a block it never finished
    /// filling, so a partial block carries no cache state to match on. Hashing
    /// it would claim a shared prefix that the worker cannot honour, which
    /// inflates every predicted hit rate by up to one block per request.
    #[test]
    fn a_difference_inside_the_trailing_partial_block_does_not_reach_the_chain() {
        let builder = PromptBuilder::simple(16);
        let system = Message::new("system", words(128));

        let left = builder
            .fingerprint_chat(&[system.clone(), Message::new("user", "left question")])
            .expect("should fingerprint");
        let right = builder
            .fingerprint_chat(&[system, Message::new("user", "right question")])
            .expect("should fingerprint");

        assert_eq!(left.blocks, right.blocks);
        assert_ne!(
            left.token_count % 16,
            0,
            "this test is only meaningful when the prompt ends mid-block"
        );
    }

    #[test]
    fn conversations_with_nothing_in_common_share_no_blocks() {
        let builder = PromptBuilder::simple(16);

        let left = builder
            .fingerprint_chat(&[Message::new("user", words(64))])
            .expect("should fingerprint");
        let right = builder
            .fingerprint_chat(&[Message::new("user", "totally different text here")])
            .expect("should fingerprint");

        assert_eq!(shared_prefix_len(&left.blocks, &right.blocks), 0);
    }

    #[test]
    fn a_short_prompt_fingerprints_to_no_blocks() {
        let builder = PromptBuilder::simple(16);
        let fingerprint = builder
            .fingerprint_chat(&[Message::new("user", "hi")])
            .expect("should fingerprint");

        assert!(fingerprint.is_empty());
        assert!(fingerprint.token_count > 0);
    }

    #[test]
    fn text_and_chat_endpoints_use_the_same_hashing() {
        let builder = PromptBuilder::simple(16);
        let rendered = ChatTemplate::Simple
            .render(&[Message::new("user", words(64))], true)
            .expect("should render");

        assert_eq!(
            builder.fingerprint_text(&rendered).blocks,
            builder
                .fingerprint_chat(&[Message::new("user", words(64))])
                .expect("should fingerprint")
                .blocks
        );
    }

    #[test]
    fn the_tokenizer_name_is_reported_for_the_manifest() {
        assert_eq!(PromptBuilder::simple(16).tokenizer_name(), "words");
        assert_eq!(PromptBuilder::simple(16).block_size(), 16);
    }
}
