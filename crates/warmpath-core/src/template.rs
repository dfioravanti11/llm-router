//! Rendering a conversation into the text the worker will tokenize.
//!
//! This module exists because of one specific bug. SGLang's prefill/decode
//! router built its cache-aware routing text from only the first chat message.
//! Nothing failed. Single-turn traffic matched fine, and multi-turn traffic
//! quietly matched on a fraction of the prompt, so the router looked like it
//! worked and the hit rate was wrong in a way no error could reveal.
//!
//! Every renderer here takes the whole conversation. The test suite asserts
//! that every message reaches the output, which is the assertion that would
//! have caught it.

use minijinja::Environment;
use serde::{Deserialize, Serialize};

/// One turn of a conversation, in the shape OpenAI clients send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: String,
}

impl Message {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("chat template failed to parse: {0}")]
    Parse(String),
    #[error("chat template failed to render: {0}")]
    Render(String),
}

/// How a conversation becomes prompt text.
#[derive(Debug)]
pub enum ChatTemplate {
    /// A plain, documented rendering used against the mock worker.
    ///
    /// Each message becomes `<|role|>\n{content}\n`, then a trailing
    /// `<|assistant|>\n` marks where generation starts. No model uses exactly
    /// this, and it does not need to: against a mock worker the only thing that
    /// matters is that the same conversation always renders the same way and
    /// that a conversation extending another shares its leading text.
    Simple,
    /// A Jinja template, in the form a model ships in `tokenizer_config.json`.
    Jinja(Box<Environment<'static>>),
}

impl ChatTemplate {
    /// Compile a model's chat template.
    pub fn jinja(source: &str) -> Result<Self, TemplateError> {
        let mut environment = Environment::new();
        environment
            .add_template_owned("chat", source.to_string())
            .map_err(|err| TemplateError::Parse(err.to_string()))?;
        Ok(ChatTemplate::Jinja(Box::new(environment)))
    }

    /// Render the full conversation.
    ///
    /// `add_generation_prompt` mirrors the argument every model's template
    /// takes, and marks that the assistant turn is about to begin.
    pub fn render(
        &self,
        messages: &[Message],
        add_generation_prompt: bool,
    ) -> Result<String, TemplateError> {
        match self {
            ChatTemplate::Simple => Ok(render_simple(messages, add_generation_prompt)),
            ChatTemplate::Jinja(environment) => {
                let template = environment
                    .get_template("chat")
                    .map_err(|err| TemplateError::Render(err.to_string()))?;
                template
                    .render(minijinja::context! {
                        messages => messages,
                        add_generation_prompt => add_generation_prompt,
                    })
                    .map_err(|err| TemplateError::Render(err.to_string()))
            }
        }
    }
}

fn render_simple(messages: &[Message], add_generation_prompt: bool) -> String {
    let mut rendered = String::with_capacity(
        messages
            .iter()
            .map(|message| message.role.len() + message.content.len() + 16)
            .sum::<usize>()
            + 16,
    );

    for message in messages {
        rendered.push_str("<|");
        rendered.push_str(&message.role);
        rendered.push_str("|>\n");
        rendered.push_str(&message.content);
        rendered.push('\n');
    }

    if add_generation_prompt {
        rendered.push_str("<|assistant|>\n");
    }

    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like the templates models actually ship.
    const QWEN_LIKE: &str = concat!(
        "{% for message in messages %}",
        "<|im_start|>{{ message.role }}\n{{ message.content }}<|im_end|>\n",
        "{% endfor %}",
        "{% if add_generation_prompt %}<|im_start|>assistant\n{% endif %}"
    );

    fn conversation() -> Vec<Message> {
        vec![
            Message::new("system", "You are a router."),
            Message::new("user", "First question."),
            Message::new("assistant", "First answer."),
            Message::new("user", "Second question."),
        ]
    }

    #[test]
    fn the_simple_template_includes_every_message() {
        let rendered = ChatTemplate::Simple
            .render(&conversation(), true)
            .expect("should render");

        for message in conversation() {
            assert!(
                rendered.contains(&message.content),
                "message missing from render: {}\n{rendered}",
                message.content
            );
        }
    }

    #[test]
    fn a_jinja_template_includes_every_message() {
        let template = ChatTemplate::jinja(QWEN_LIKE).expect("template should compile");
        let rendered = template
            .render(&conversation(), true)
            .expect("should render");

        for message in conversation() {
            assert!(
                rendered.contains(&message.content),
                "message missing from render: {}\n{rendered}",
                message.content
            );
        }
        assert!(rendered.ends_with("<|im_start|>assistant\n"), "{rendered}");
    }

    /// The SGLang bug, as a regression test.
    ///
    /// A four-message conversation must not render to the same text as its
    /// first message alone. If it did, every turn after the first would route
    /// on a prefix that ignores the conversation, and nothing would report an
    /// error.
    #[test]
    fn a_conversation_does_not_render_like_its_first_message() {
        for template in [
            ChatTemplate::Simple,
            ChatTemplate::jinja(QWEN_LIKE).expect("template should compile"),
        ] {
            let full = template
                .render(&conversation(), true)
                .expect("should render");
            let first_only = template
                .render(&conversation()[..1], true)
                .expect("should render");

            assert_ne!(full, first_only);
            assert!(full.len() > first_only.len() * 2);
        }
    }

    /// The property prefix caching lives on: turn N+1 must extend turn N's
    /// text, not rewrite it.
    #[test]
    fn a_later_turn_extends_the_text_of_an_earlier_one() {
        for template in [
            ChatTemplate::Simple,
            ChatTemplate::jinja(QWEN_LIKE).expect("template should compile"),
        ] {
            let history = &conversation()[..3];

            // The prefix shared between turns is the history without the
            // generation prompt, since that marker sits at the end.
            let earlier = template.render(history, false).expect("should render");
            let later = template
                .render(&conversation(), false)
                .expect("should render");

            assert!(
                later.starts_with(&earlier),
                "turn N+1 did not extend turn N\nearlier: {earlier:?}\nlater: {later:?}"
            );
        }
    }

    #[test]
    fn the_generation_prompt_is_optional() {
        let with = ChatTemplate::Simple
            .render(&conversation(), true)
            .expect("should render");
        let without = ChatTemplate::Simple
            .render(&conversation(), false)
            .expect("should render");

        assert!(with.starts_with(&without));
        assert!(with.len() > without.len());
    }

    #[test]
    fn rendering_is_deterministic() {
        let template = ChatTemplate::jinja(QWEN_LIKE).expect("template should compile");
        assert_eq!(
            template
                .render(&conversation(), true)
                .expect("should render"),
            template
                .render(&conversation(), true)
                .expect("should render")
        );
    }

    #[test]
    fn an_empty_conversation_renders_to_the_generation_prompt_alone() {
        let rendered = ChatTemplate::Simple
            .render(&[], true)
            .expect("should render");
        assert_eq!(rendered, "<|assistant|>\n");
    }

    #[test]
    fn a_broken_template_is_reported_rather_than_panicking() {
        let error = ChatTemplate::jinja("{% for message in messages %}").expect_err("should fail");
        assert!(matches!(error, TemplateError::Parse(_)), "{error:?}");
    }
}
