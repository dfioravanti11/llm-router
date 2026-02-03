//! Request forwarding.
//!
//! The response body is streamed through untouched. Nothing here parses SSE,
//! re-chunks, or decompresses, so what the client reads is byte-for-byte what
//! the worker wrote. Backpressure works because the upstream byte stream is
//! polled inline by the client's response body: bytes are pulled off the worker
//! socket only when the client is ready for them.

use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;

use crate::error::ProxyError;
use crate::AppState;

/// Header carrying the id assigned at ingress. Sent upstream and returned to
/// the client so one id spans the whole path.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-warmpath-request-id");
/// Header naming the worker that served the request.
pub const WORKER_HEADER: HeaderName = HeaderName::from_static("x-warmpath-worker");

/// Longest client-supplied request id accepted before one is generated instead.
const MAX_INBOUND_REQUEST_ID: usize = 128;

pub async fn proxy(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ProxyError> {
    let (parts, body) = request.into_parts();
    let request_id = resolve_request_id(&parts.headers, &state);
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string();

    // Reject on the declared length before buffering. A chunked body that
    // overruns the limit is caught below and reported as a bad request, since
    // `to_bytes` does not distinguish overrun from a read failure.
    if let Some(declared) = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
    {
        if declared > state.max_request_bytes {
            return Err(ProxyError::BodyTooLarge {
                limit: state.max_request_bytes,
            });
        }
    }

    let body_bytes = axum::body::to_bytes(body, state.max_request_bytes)
        .await
        .map_err(|err| ProxyError::RequestBody(err.to_string()))?;

    let worker = state.pool.pick();
    let url = worker.endpoint(&path_and_query);

    let mut upstream_headers = HeaderMap::with_capacity(parts.headers.len() + 1);
    copy_forwardable_headers(&parts.headers, &mut upstream_headers);
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        upstream_headers.insert(REQUEST_ID_HEADER, value);
    }

    tracing::info!(
        request_id = %request_id,
        worker = %worker.name,
        method = %parts.method,
        path = %path_and_query,
        request_bytes = body_bytes.len(),
        "dispatching request"
    );

    let dispatched_at = Instant::now();
    let upstream = state
        .pool
        .client()
        .request(parts.method.clone(), &url)
        .headers(upstream_headers)
        .body(body_bytes)
        .send()
        .await
        .map_err(|source| ProxyError::Upstream {
            worker: worker.name.clone(),
            source,
        })?;

    let status = upstream.status();
    let mut response_headers = HeaderMap::with_capacity(upstream.headers().len() + 2);
    copy_forwardable_headers(upstream.headers(), &mut response_headers);
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response_headers.insert(REQUEST_ID_HEADER, value);
    }
    if let Ok(value) = HeaderValue::from_str(&worker.name) {
        response_headers.insert(WORKER_HEADER, value);
    }

    let guard = StreamGuard {
        request_id: request_id.clone(),
        worker: worker.name.clone(),
        dispatched_at,
        finished: false,
    };

    let stream = async_stream::stream! {
        // The guard and the upstream response both live inside this generator.
        // If the client goes away, axum drops the response body, which drops
        // the generator, which closes the upstream connection and records the
        // cancellation. There is no separate cancellation path to keep in sync.
        let mut guard = guard;
        let mut upstream_body = upstream.bytes_stream();
        let mut seen_first_chunk = false;

        while let Some(chunk) = upstream_body.next().await {
            match chunk {
                Ok(bytes) => {
                    if !seen_first_chunk {
                        seen_first_chunk = true;
                        guard.record_first_byte();
                    }
                    yield Ok::<Bytes, std::io::Error>(bytes);
                }
                Err(err) => {
                    guard.record_failure(&err);
                    yield Err(std::io::Error::other(err));
                    return;
                }
            }
        }

        guard.record_completion();
    };

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    Ok(response)
}

pub async fn health() -> &'static str {
    "ok"
}

/// Tracks the fate of one streamed response.
///
/// Dropping this without a terminal call means the body was never finished,
/// which is how a client disconnect is detected.
struct StreamGuard {
    request_id: String,
    worker: String,
    dispatched_at: Instant,
    finished: bool,
}

impl StreamGuard {
    fn record_first_byte(&self) {
        tracing::debug!(
            request_id = %self.request_id,
            worker = %self.worker,
            ttfb_ms = self.dispatched_at.elapsed().as_secs_f64() * 1000.0,
            "first byte from worker"
        );
    }

    fn record_completion(&mut self) {
        self.finished = true;
        tracing::info!(
            request_id = %self.request_id,
            worker = %self.worker,
            duration_ms = self.dispatched_at.elapsed().as_secs_f64() * 1000.0,
            "response complete"
        );
    }

    fn record_failure(&mut self, error: &reqwest::Error) {
        self.finished = true;
        tracing::warn!(
            request_id = %self.request_id,
            worker = %self.worker,
            error = %error,
            "upstream stream failed"
        );
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        if !self.finished {
            tracing::info!(
                request_id = %self.request_id,
                worker = %self.worker,
                duration_ms = self.dispatched_at.elapsed().as_secs_f64() * 1000.0,
                "client disconnected; upstream request cancelled"
            );
        }
    }
}

fn resolve_request_id(headers: &HeaderMap, state: &AppState) -> String {
    let inbound = headers
        .get("x-request-id")
        .or_else(|| headers.get(&REQUEST_ID_HEADER))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_INBOUND_REQUEST_ID
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b'-' || byte == b'_')
        });

    match inbound {
        Some(value) => value.to_string(),
        None => state.request_ids.next(),
    }
}

/// Copy every header that is safe to forward across a proxy hop.
///
/// Drops hop-by-hop headers, anything the peer listed in `Connection`, and the
/// two headers the outgoing HTTP client owns (`host`, `content-length`).
fn copy_forwardable_headers(source: &HeaderMap, destination: &mut HeaderMap) {
    let connection_tokens: Vec<String> = source
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect();

    for (name, value) in source.iter() {
        let lowercase = name.as_str();
        if is_hop_by_hop(lowercase) || connection_tokens.iter().any(|token| token == lowercase) {
            continue;
        }
        destination.append(name.clone(), value.clone());
    }
}

/// Header names that describe a single connection rather than the message, and
/// so must not cross a proxy hop. `host` and `content-length` are included
/// because the outgoing client sets both from the request it is building.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "content-length"
            | "host"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::try_from(*name).expect("valid header name"),
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        map
    }

    #[test]
    fn hop_by_hop_headers_are_dropped() {
        let source = header_map(&[
            ("host", "example.com"),
            ("content-length", "12"),
            ("transfer-encoding", "chunked"),
            ("connection", "keep-alive"),
            ("content-type", "application/json"),
        ]);

        let mut destination = HeaderMap::new();
        copy_forwardable_headers(&source, &mut destination);

        assert_eq!(destination.len(), 1);
        assert_eq!(destination["content-type"], "application/json");
    }

    #[test]
    fn headers_named_by_connection_are_dropped() {
        let source = header_map(&[
            ("connection", "x-custom, close"),
            ("x-custom", "should-not-cross"),
            ("authorization", "Bearer token"),
        ]);

        let mut destination = HeaderMap::new();
        copy_forwardable_headers(&source, &mut destination);

        assert!(destination.get("x-custom").is_none());
        assert_eq!(destination["authorization"], "Bearer token");
    }

    #[test]
    fn repeated_headers_are_preserved() {
        let source = header_map(&[("set-cookie", "a=1"), ("set-cookie", "b=2")]);

        let mut destination = HeaderMap::new();
        copy_forwardable_headers(&source, &mut destination);

        let cookies: Vec<_> = destination.get_all("set-cookie").iter().collect();
        assert_eq!(cookies.len(), 2);
    }
}
