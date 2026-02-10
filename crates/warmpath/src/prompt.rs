//! Reading the prompt out of an OpenAI request body.
//!
//! Parsing is deliberately forgiving. A body the router cannot understand is
//! still a body a worker might serve perfectly well, so an unparseable request
//! loses its cache affinity and routes on load. Rejecting it would turn a
//! routing optimization into an availability risk, which inverts the whole
//! point.

use serde::Deserialize;
use warmpath_core::{Message, PromptBuilder, PromptFingerprint};

/// The parts of a completion request that decide where it should go.
#[derive(Debug, Deserialize)]
struct IncomingRequest {
    /// Chat completions.
    #[serde(default)]
    messages: Option<Vec<Message>>,
    /// Legacy text completions. May be a string or a list of strings; token-id
    /// prompts are ignored, since the router has no vocabulary to render them.
    #[serde(default)]
    prompt: Option<PromptField>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PromptField {
    Text(String),
    Batch(Vec<String>),
    /// Token ids, or anything else this router does not read. The value is
    /// matched but never inspected, because there is no vocabulary here to
    /// render it back into text.
    Other(serde::de::IgnoredAny),
}

/// Fingerprint a request body, or return `None` if there is nothing to route on.
pub fn fingerprint(builder: &PromptBuilder, body: &[u8]) -> Option<PromptFingerprint> {
    let request: IncomingRequest = serde_json::from_slice(body).ok()?;

    if let Some(messages) = request.messages {
        if messages.is_empty() {
            return None;
        }
        // The whole conversation, never just the newest message.
        return builder.fingerprint_chat(&messages).ok();
    }

    match request.prompt? {
        PromptField::Text(text) => Some(builder.fingerprint_text(&text)),
        // A batch shares its cache behaviour with its first element, which is
        // the only part every response in the batch has in common.
        PromptField::Batch(items) => items.first().map(|text| builder.fingerprint_text(text)),
        PromptField::Other(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn builder() -> PromptBuilder {
        PromptBuilder::simple(16)
    }

    fn words(count: usize) -> String {
        (0..count)
            .map(|index| format!("w{index}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn a_chat_request_fingerprints_its_whole_conversation() {
        let body = json!({
            "model": "mock-model",
            "messages": [
                { "role": "system", "content": words(64) },
                { "role": "user", "content": words(64) },
            ],
        });

        let fingerprinted = fingerprint(&builder(), &serde_json::to_vec(&body).unwrap())
            .expect("should fingerprint");
        let expected = builder()
            .fingerprint_chat(&[
                Message::new("system", words(64)),
                Message::new("user", words(64)),
            ])
            .expect("should fingerprint");

        assert_eq!(fingerprinted, expected);
        assert!(fingerprinted.block_count() > 4);
    }

    /// The SGLang bug once more, this time at the parsing boundary.
    #[test]
    fn a_longer_conversation_fingerprints_differently_from_its_first_message() {
        let first_only = json!({
            "messages": [{ "role": "system", "content": words(64) }],
        });
        let full = json!({
            "messages": [
                { "role": "system", "content": words(64) },
                { "role": "user", "content": words(64) },
            ],
        });

        let short = fingerprint(&builder(), &serde_json::to_vec(&first_only).unwrap())
            .expect("should fingerprint");
        let long = fingerprint(&builder(), &serde_json::to_vec(&full).unwrap())
            .expect("should fingerprint");

        assert!(long.block_count() > short.block_count());
    }

    #[test]
    fn a_text_completion_fingerprints_its_prompt() {
        let body = json!({ "model": "mock-model", "prompt": words(64) });
        let fingerprinted = fingerprint(&builder(), &serde_json::to_vec(&body).unwrap())
            .expect("should fingerprint");

        assert_eq!(fingerprinted, builder().fingerprint_text(&words(64)));
    }

    #[test]
    fn a_batch_prompt_fingerprints_its_first_entry() {
        let body = json!({ "prompt": [words(64), "something else entirely"] });
        let fingerprinted = fingerprint(&builder(), &serde_json::to_vec(&body).unwrap())
            .expect("should fingerprint");

        assert_eq!(fingerprinted, builder().fingerprint_text(&words(64)));
    }

    #[test]
    fn unusable_bodies_yield_nothing_rather_than_failing() {
        for body in [
            json!({ "model": "mock-model" }),
            json!({ "messages": [] }),
            json!({ "prompt": [[1, 2, 3]] }),
            json!({ "prompt": 42 }),
            json!("not an object"),
        ] {
            assert!(
                fingerprint(&builder(), &serde_json::to_vec(&body).unwrap()).is_none(),
                "expected no fingerprint for {body}"
            );
        }
    }

    #[test]
    fn malformed_json_yields_nothing_rather_than_failing() {
        assert!(fingerprint(&builder(), b"{ not json").is_none());
        assert!(fingerprint(&builder(), b"").is_none());
    }

    #[test]
    fn unknown_fields_do_not_stop_the_prompt_being_read() {
        // Real clients send sampling parameters, tools, and vendor extensions.
        // None of them are the router's business.
        let body = json!({
            "model": "mock-model",
            "messages": [{ "role": "user", "content": words(64) }],
            "temperature": 0.7,
            "tools": [{ "type": "function" }],
            "some_vendor_extension": { "nested": true },
        });

        assert!(fingerprint(&builder(), &serde_json::to_vec(&body).unwrap()).is_some());
    }

    #[test]
    fn a_message_without_content_is_still_part_of_the_conversation() {
        let body = json!({
            "messages": [
                { "role": "user", "content": words(64) },
                { "role": "assistant" },
            ],
        });

        assert!(fingerprint(&builder(), &serde_json::to_vec(&body).unwrap()).is_some());
    }
}
