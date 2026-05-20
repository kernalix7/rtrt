//! Shared SSE helpers for streaming chat responses.
//!
//! Both Anthropic and OpenAI use Server-Sent Events; the parsing shape differs but
//! the framing (`data: <json>\n\n`) is identical. This module decodes the byte
//! stream into typed [`ChatStreamEvent`]s once a provider supplies the per-event
//! JSON shape.

use std::pin::Pin;

use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use rtrt_core::{Error, Result};

use crate::ChatStreamEvent;

pub type EventStream = Pin<Box<dyn Stream<Item = Result<ChatStreamEvent>> + Send>>;

pub fn decode<F>(response: reqwest::Response, mut handler: F) -> EventStream
where
    F: FnMut(&str, &str) -> Result<Option<ChatStreamEvent>> + Send + 'static,
{
    let stream = response
        .bytes_stream()
        .eventsource()
        .filter_map(move |evt| {
            let result = match evt {
                Ok(evt) => match handler(&evt.event, &evt.data) {
                    Ok(Some(ev)) => Some(Ok(ev)),
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                },
                Err(e) => Some(Err(Error::Provider(format!("sse error: {e}")))),
            };
            std::future::ready(result)
        });
    Box::pin(stream)
}
