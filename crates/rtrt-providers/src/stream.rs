//! Shared SSE helpers for streaming chat responses.
//!
//! Both Anthropic and OpenAI use Server-Sent Events; the parsing shape differs but
//! the framing (`data: <json>\n\n`) is identical. This module decodes the byte
//! stream into typed [`ChatStreamEvent`]s once a provider supplies the per-event
//! JSON shape.

use std::pin::Pin;

use futures_util::Stream;
use rtrt_core::{Error, Result};

use crate::ChatStreamEvent;

pub type EventStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent>> + Send>>;

/// Unary JSON responses may be large (roughly four million UTF-8 characters),
/// but must not turn a broken endpoint into an unbounded allocation.
pub(crate) const MAX_SUCCESS_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Provider error payloads are diagnostic only; 256 KiB is ample for them.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 256 * 1024;
/// One SSE event normally contains one token delta. 1 MiB also accommodates
/// unusually large tool/result deltas without permitting an unbounded frame.
pub(crate) const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
/// A stream may carry a long model answer; cap its wire representation at 64 MiB.
pub(crate) const MAX_SSE_TOTAL_BYTES: usize = 64 * 1024 * 1024;

pub(crate) async fn read_body_bounded(
    mut response: reqwest::Response,
    provider: &str,
    kind: &str,
    limit: usize,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(body_limit_error(provider, kind, limit));
    }
    let capacity = response.content_length().unwrap_or(0).min(limit as u64) as usize;
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::Provider(format!("{provider} {kind} body read failed")))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(body_limit_error(provider, kind, limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn body_limit_error(provider: &str, kind: &str, limit: usize) -> Error {
    Error::Provider(format!("{provider} {kind} body exceeds {limit} byte limit"))
}

struct SseState<F> {
    response: reqwest::Response,
    buffer: Vec<u8>,
    total: usize,
    provider: &'static str,
    terminal_error: Option<Error>,
    finished: bool,
    handler: F,
}

pub fn decode<F>(response: reqwest::Response, provider: &'static str, handler: F) -> EventStream
where
    F: FnMut(&str, &str) -> Result<Option<ChatStreamEvent>> + Send + 'static,
{
    let oversized = response
        .content_length()
        .is_some_and(|length| length > MAX_SSE_TOTAL_BYTES as u64)
        .then(|| sse_limit_error(provider, "total", MAX_SSE_TOTAL_BYTES));
    let state = SseState {
        response,
        buffer: Vec::new(),
        total: 0,
        provider,
        terminal_error: oversized,
        finished: false,
        handler,
    };
    let stream = futures_util::stream::unfold(state, move |mut state| async {
        loop {
            if state.finished {
                return None;
            }
            if let Some(error) = state.terminal_error.take() {
                state.finished = true;
                return Some((Err(error), state));
            }
            if let Some(end) = event_boundary(&state.buffer) {
                if end > MAX_SSE_EVENT_BYTES {
                    state.buffer.clear();
                    state.finished = true;
                    return Some((
                        Err(sse_limit_error(
                            state.provider,
                            "event",
                            MAX_SSE_EVENT_BYTES,
                        )),
                        state,
                    ));
                }
                let frame: Vec<u8> = state.buffer.drain(..end).collect();
                if let Some((event, data)) = parse_event(&frame) {
                    return match (state.handler)(&event, &data) {
                        Ok(Some(value)) => Some((Ok(value), state)),
                        Ok(None) => continue,
                        Err(error) => Some((Err(error), state)),
                    };
                }
                continue;
            }
            if state.buffer.len() > MAX_SSE_EVENT_BYTES {
                state.buffer.clear();
                state.finished = true;
                return Some((
                    Err(sse_limit_error(
                        state.provider,
                        "event",
                        MAX_SSE_EVENT_BYTES,
                    )),
                    state,
                ));
            }
            match state.response.chunk().await {
                Ok(Some(chunk)) => {
                    let next_total = state.total.saturating_add(chunk.len());
                    if next_total > MAX_SSE_TOTAL_BYTES {
                        state.finished = true;
                        return Some((
                            Err(sse_limit_error(
                                state.provider,
                                "total",
                                MAX_SSE_TOTAL_BYTES,
                            )),
                            state,
                        ));
                    }
                    state.total = next_total;
                    state.buffer.extend_from_slice(&chunk);
                }
                Ok(None) if state.buffer.is_empty() => return None,
                Ok(None) => {
                    if state.buffer.len() > MAX_SSE_EVENT_BYTES {
                        state.finished = true;
                        return Some((
                            Err(sse_limit_error(
                                state.provider,
                                "event",
                                MAX_SSE_EVENT_BYTES,
                            )),
                            state,
                        ));
                    }
                    let frame = std::mem::take(&mut state.buffer);
                    if let Some((event, data)) = parse_event(&frame) {
                        return match (state.handler)(&event, &data) {
                            Ok(Some(value)) => Some((Ok(value), state)),
                            Ok(None) => None,
                            Err(error) => Some((Err(error), state)),
                        };
                    }
                    return None;
                }
                Err(_) => {
                    state.finished = true;
                    return Some((
                        Err(Error::Provider(format!(
                            "{} SSE read failed",
                            state.provider
                        ))),
                        state,
                    ));
                }
            }
        }
    });
    Box::pin(stream)
}

fn sse_limit_error(provider: &str, scope: &str, limit: usize) -> Error {
    Error::Provider(format!("{provider} SSE {scope} exceeds {limit} byte limit"))
}

fn event_boundary(bytes: &[u8]) -> Option<usize> {
    let mut line_start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            let line = &bytes[line_start..index];
            if line.is_empty() || line == b"\r" {
                return Some(index + 1);
            }
            line_start = index + 1;
        }
    }
    None
}

fn parse_event(frame: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(frame);
    let mut event = "message".to_string();
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            event = value.strip_prefix(' ').unwrap_or(value).to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    (!data.is_empty()).then(|| (event, data.join("\n")))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use futures_util::StreamExt;

    use super::*;

    fn raw_server(response: Vec<u8>) -> Option<(String, std::thread::JoinHandle<()>)> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
        let address = listener.local_addr().ok()?;
        let handle = std::thread::spawn(move || {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(&response);
        });
        Some((format!("http://{address}"), handle))
    }

    #[tokio::test]
    async fn rejects_oversized_content_length_before_body_allocation() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_SUCCESS_BODY_BYTES + 1
        )
        .into_bytes();
        let Some((url, server)) = raw_server(response) else {
            return;
        };
        let response = reqwest::get(url).await.unwrap();
        let error = read_body_bounded(response, "openai", "success", MAX_SUCCESS_BODY_BYTES)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "provider error: openai success body exceeds {} byte limit",
                MAX_SUCCESS_BODY_BYTES
            )
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_oversized_chunked_body_without_returning_partial_bytes() {
        let body = vec![b'x'; MAX_ERROR_BODY_BYTES + 1];
        let mut response = b"HTTP/1.1 500 Error\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        response.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        response.extend_from_slice(&body);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let Some((url, server)) = raw_server(response) else {
            return;
        };
        let response = reqwest::get(url).await.unwrap();
        let error = read_body_bounded(response, "anthropic", "error", MAX_ERROR_BODY_BYTES)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("anthropic error body exceeds"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_oversized_chunked_sse_event_with_sanitized_error() {
        let data = vec![b'x'; MAX_SSE_EVENT_BYTES + 1];
        let mut frame = b"data: ".to_vec();
        frame.extend_from_slice(&data);
        frame.extend_from_slice(b"\n\n");
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n".to_vec();
        response.extend_from_slice(format!("{:x}\r\n", frame.len()).as_bytes());
        response.extend_from_slice(&frame);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let Some((url, server)) = raw_server(response) else {
            return;
        };
        let response = reqwest::get(url).await.unwrap();
        let mut events = decode(response, "openai", |_, _| Ok(None));
        let error = events.next().await.unwrap().unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "provider error: openai SSE event exceeds {} byte limit",
                MAX_SSE_EVENT_BYTES
            )
        );
        assert!(events.next().await.is_none());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_sse_total_from_content_length() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/event-stream\r\n\r\n",
            MAX_SSE_TOTAL_BYTES + 1
        )
        .into_bytes();
        let Some((url, server)) = raw_server(response) else {
            return;
        };
        let response = reqwest::get(url).await.unwrap();
        let mut events = decode(response, "anthropic", |_, _| Ok(None));
        let error = events.next().await.unwrap().unwrap_err();
        assert!(error.to_string().contains("anthropic SSE total exceeds"));
        assert!(events.next().await.is_none());
        server.join().unwrap();
    }
}
