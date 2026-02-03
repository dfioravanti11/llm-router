//! OpenAI-compatible completion endpoints.
//!
//! Output is deterministic on purpose: the completion id is fixed and `created`
//! is zero, so two runs of the same request produce byte-identical responses.
//! That is what lets the router's tests assert byte-identical passthrough
//! instead of merely comparing decoded fields.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{MockState, Slot};

/// Fixed id so responses are reproducible across runs.
const COMPLETION_ID: &str = "cmpl-warmpath-mock";

/// Cycled to produce token content. Deterministic, and long enough that the
/// repeat period is not mistaken for a framing bug when reading a stream.
const TOKENS: &[&str] = &[
    "the",
    "cache",
    "is",
    "warm",
    "when",
    "the",
    "prefix",
    "already",
    "lives",
    "on",
    "this",
    "worker",
    "and",
    "cold",
    "otherwise",
];

#[derive(Debug, Deserialize)]
struct CompletionRequest {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    max_tokens: Option<usize>,
}

/// Which wire shape to emit. The two OpenAI endpoints differ only in how a
/// token is carried inside the choice object.
#[derive(Debug, Clone, Copy)]
enum Flavor {
    Chat,
    Text,
}

impl Flavor {
    fn object(self, streaming: bool) -> &'static str {
        match (self, streaming) {
            (Flavor::Chat, true) => "chat.completion.chunk",
            (Flavor::Chat, false) => "chat.completion",
            (Flavor::Text, _) => "text_completion",
        }
    }
}

pub async fn chat_completions(
    state: State<MockState>,
    body: Json<Value>,
) -> Result<Response, ErrorResponse> {
    complete(state, body, Flavor::Chat).await
}

pub async fn completions(
    state: State<MockState>,
    body: Json<Value>,
) -> Result<Response, ErrorResponse> {
    complete(state, body, Flavor::Text).await
}

async fn complete(
    State(state): State<MockState>,
    Json(raw): Json<Value>,
    flavor: Flavor,
) -> Result<Response, ErrorResponse> {
    let request: CompletionRequest = serde_json::from_value(raw)
        .map_err(|err| ErrorResponse::bad_request(format!("invalid request: {err}")))?;

    let config = state.config();
    let model = request.model.unwrap_or_else(|| config.model.clone());
    let token_count = request.max_tokens.unwrap_or(config.default_max_tokens);
    let slot = state.admit();

    if request.stream {
        Ok(stream_response(
            state.clone(),
            slot,
            flavor,
            model,
            token_count,
        ))
    } else {
        Ok(buffered_response(state.clone(), slot, flavor, model, token_count).await)
    }
}

fn stream_response(
    state: MockState,
    slot: Slot,
    flavor: Flavor,
    model: String,
    token_count: usize,
) -> Response {
    let ttft = state.config().time_to_first_token;
    let inter_token = state.config().inter_token_delay;

    let body = async_stream::stream! {
        // `slot` is owned by the generator, so dropping the response body
        // releases it. No cancellation plumbing needed.
        let mut slot = slot;

        tokio::time::sleep(ttft).await;
        yield Ok::<Bytes, std::io::Error>(sse_frame(&chunk(flavor, &model, Delta::Open)));

        for index in 0..token_count {
            if index > 0 {
                tokio::time::sleep(inter_token).await;
            }
            let token = token_at(index);
            yield Ok(sse_frame(&chunk(flavor, &model, Delta::Token(&token))));
        }

        tokio::time::sleep(inter_token).await;
        yield Ok(sse_frame(&chunk(flavor, &model, Delta::Stop)));
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));

        slot.complete();
    };

    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

async fn buffered_response(
    state: MockState,
    mut slot: Slot,
    flavor: Flavor,
    model: String,
    token_count: usize,
) -> Response {
    let config = state.config();
    let elapsed = config.time_to_first_token + config.inter_token_delay * token_count as u32;
    tokio::time::sleep(elapsed).await;

    let text: String = (0..token_count).map(token_at).collect();
    let choice = match flavor {
        Flavor::Chat => json!({
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }),
        Flavor::Text => json!({
            "index": 0,
            "text": text,
            "finish_reason": "stop",
        }),
    };
    let payload = json!({
        "id": COMPLETION_ID,
        "object": flavor.object(false),
        "created": 0,
        "model": model,
        "choices": [choice],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": token_count,
            "total_tokens": token_count,
        },
    });

    slot.complete();
    Json(payload).into_response()
}

/// What a single streamed chunk carries.
enum Delta<'a> {
    /// Opening chunk: announces the assistant role, no content yet.
    Open,
    Token(&'a str),
    Stop,
}

fn chunk(flavor: Flavor, model: &str, delta: Delta<'_>) -> Value {
    let choice = match (flavor, delta) {
        (Flavor::Chat, Delta::Open) => json!({
            "index": 0,
            "delta": { "role": "assistant", "content": "" },
            "finish_reason": Value::Null,
        }),
        (Flavor::Chat, Delta::Token(token)) => json!({
            "index": 0,
            "delta": { "content": token },
            "finish_reason": Value::Null,
        }),
        (Flavor::Chat, Delta::Stop) => json!({
            "index": 0,
            "delta": {},
            "finish_reason": "stop",
        }),
        (Flavor::Text, Delta::Open) => json!({
            "index": 0,
            "text": "",
            "finish_reason": Value::Null,
        }),
        (Flavor::Text, Delta::Token(token)) => json!({
            "index": 0,
            "text": token,
            "finish_reason": Value::Null,
        }),
        (Flavor::Text, Delta::Stop) => json!({
            "index": 0,
            "text": "",
            "finish_reason": "stop",
        }),
    };

    json!({
        "id": COMPLETION_ID,
        "object": flavor.object(true),
        "created": 0,
        "model": model,
        "choices": [choice],
    })
}

fn sse_frame(payload: &Value) -> Bytes {
    let mut frame = String::with_capacity(192);
    frame.push_str("data: ");
    // A serde_json::Value always serializes, so the error case is unreachable.
    frame.push_str(&serde_json::to_string(payload).unwrap_or_default());
    frame.push_str("\n\n");
    Bytes::from(frame)
}

fn token_at(index: usize) -> String {
    let word = TOKENS[index % TOKENS.len()];
    if index == 0 {
        word.to_string()
    } else {
        format!(" {word}")
    }
}

/// Error body in the shape OpenAI clients expect.
pub struct ErrorResponse {
    status: StatusCode,
    message: String,
}

impl ErrorResponse {
    fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "message": self.message,
                "type": "invalid_request_error",
            }
        });
        (self.status, Json(body)).into_response()
    }
}
