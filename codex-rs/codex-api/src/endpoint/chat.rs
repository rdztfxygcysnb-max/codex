//! Chat Completions endpoint client.
//!
//! POSTs an OpenAI-compatible Chat Completions request to `{base_url}/chat/completions`
//! and adapts the SSE stream into codex's `ResponseEvent` stream. This restores the
//! `wire_api = "chat"` transport that upstream removed in #10157, with relay-friendly
//! quirks handled by `chat_wire` / `chat_provider`.

use crate::auth::SharedAuthProvider;
use crate::chat_provider::SseParser;
use crate::chat_wire::ChatEvent;
use crate::chat_wire::FinishKind;
use crate::chat_wire::HistoryItem;
use crate::chat_wire::Quirks;
use crate::chat_wire::StreamAccumulator;
use crate::chat_wire::TokenUsage as ChatTokenUsage;
use crate::chat_wire::ToolSpec;
use crate::chat_wire::build_chat_request;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use futures::StreamExt;
use http::HeaderMap;
use http::Method;
use tokio::sync::mpsc;

/// Client for OpenAI-compatible Chat Completions relays (`wire_api = "chat"`).
pub struct ChatClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> ChatClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    /// Sends one Chat Completions request and maps the SSE response stream into
    /// codex `ResponseEvent`s.
    pub async fn stream_chat(
        &self,
        model: &str,
        history: &[HistoryItem],
        tools: &[ToolSpec],
        quirks: Quirks,
        max_output_tokens: Option<u32>,
    ) -> Result<ResponseStream, ApiError> {
        let request = build_chat_request(model, history, tools, max_output_tokens, &quirks);
        let body = EncodedJsonBody::encode(&request)
            .map_err(|e| ApiError::Stream(format!("failed to encode chat request: {e}")))?;
        let response = self
            .session
            .stream_encoded_json_with(
                Method::POST,
                "/chat/completions",
                HeaderMap::new(),
                Some(body),
                |_request| {},
            )
            .await?;
        if !response.status.is_success() {
            return Err(ApiError::Api {
                status: response.status,
                message: "chat completions request failed".to_string(),
            });
        }
        Ok(spawn_chat_stream(response, quirks))
    }
}

/// Spawns a task that parses the SSE byte stream, accumulates chat events, and
/// forwards them as `ResponseEvent`s on the returned stream.
///
/// Safety contract (mirrors `chat_wire`): tool calls are only released by
/// `StreamAccumulator::finish()`, and only when the stream ended cleanly — a
/// truncated stream never executes half-written tool arguments.
fn spawn_chat_stream(response: StreamResponse, quirks: Quirks) -> ResponseStream {
    let (tx_event, rx_event) = mpsc::channel(1024);
    tokio::spawn(async move {
        // Announce the start of the stream; chat relays carry no server response id.
        if tx_event
            .send(Ok(ResponseEvent::Created { response_id: None }))
            .await
            .is_err()
        {
            return;
        }
        let mut parser = SseParser::new();
        let mut accumulator = StreamAccumulator::new(quirks);
        let mut pending_usage: Option<TokenUsage> = None;
        let mut text_state = TextItemState::default();
        let mut bytes = response.bytes;
        loop {
            let chunk = match bytes.next().await {
                Some(Ok(chunk)) => chunk,
                Some(Err(e)) => {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(format!(
                            "chat stream transport error: {e}"
                        ))))
                        .await;
                    return;
                }
                None => break,
            };
            let datas = parser.push(&chunk);
            if !forward_datas(
                &mut accumulator,
                &datas,
                &mut text_state,
                &mut pending_usage,
                &tx_event,
            )
            .await
            {
                return;
            }
        }
        // The stream ended: flush the parser, then release accumulated events.
        let datas = parser.flush();
        if !forward_datas(
            &mut accumulator,
            &datas,
            &mut text_state,
            &mut pending_usage,
            &tx_event,
        )
        .await
        {
            return;
        }
        if !accumulator.ended_cleanly() {
            let _ = tx_event
                .send(Err(ApiError::Stream(
                    "chat stream ended before completion".to_string(),
                )))
                .await;
            return;
        }
        for event in accumulator.finish() {
            if !emit_event(event, &mut text_state, &mut pending_usage, &tx_event).await {
                return;
            }
        }
    });
    ResponseStream {
        rx_event,
        upstream_request_id: None,
        interrupt: None,
    }
}

/// Tracks the open assistant text item so text deltas are always wrapped in
/// `OutputItemAdded` / `OutputItemDone` pairs (the client asserts on deltas
/// that arrive without an active item).
#[derive(Default)]
struct TextItemState {
    active: bool,
    buffer: String,
}

fn assistant_message_item(content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// Feeds parsed SSE payloads into the accumulator and forwards mapped events.
/// Returns `false` when the receiver is gone or a wire error was surfaced.
async fn forward_datas(
    accumulator: &mut StreamAccumulator,
    datas: &[String],
    text_state: &mut TextItemState,
    pending_usage: &mut Option<TokenUsage>,
    tx: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
) -> bool {
    for data in datas {
        let events = match accumulator.feed(data) {
            Ok(events) => events,
            Err(e) => {
                let _ = tx.send(Err(ApiError::Stream(e.to_string()))).await;
                return false;
            }
        };
        for event in events {
            if !emit_event(event, text_state, pending_usage, tx).await {
                return false;
            }
        }
    }
    true
}

/// Emits one chat event, keeping the assistant-text item state machine consistent.
async fn emit_event(
    event: ChatEvent,
    text_state: &mut TextItemState,
    pending_usage: &mut Option<TokenUsage>,
    tx: &mpsc::Sender<Result<ResponseEvent, ApiError>>,
) -> bool {
    match event {
        ChatEvent::TextDelta(text) => {
            if !text_state.active {
                if tx
                    .send(Ok(ResponseEvent::OutputItemAdded(assistant_message_item(
                        Vec::new(),
                    ))))
                    .await
                    .is_err()
                {
                    return false;
                }
                text_state.active = true;
            }
            text_state.buffer.push_str(&text);
            tx.send(Ok(ResponseEvent::OutputTextDelta(text)))
                .await
                .is_ok()
        }
        other => {
            if text_state.active {
                let content = vec![ContentItem::OutputText {
                    text: std::mem::take(&mut text_state.buffer),
                }];
                if tx
                    .send(Ok(ResponseEvent::OutputItemDone(assistant_message_item(
                        content,
                    ))))
                    .await
                    .is_err()
                {
                    return false;
                }
                text_state.active = false;
            }
            match to_response_event(other, pending_usage) {
                Some(mapped) => tx.send(Ok(mapped)).await.is_ok(),
                None => true,
            }
        }
    }
}

/// Maps one chat event to a codex `ResponseEvent`.
/// Usage is not a standalone event: it is stashed and attached to `Completed`.
fn to_response_event(
    event: ChatEvent,
    pending_usage: &mut Option<TokenUsage>,
) -> Option<ResponseEvent> {
    match event {
        ChatEvent::Usage(usage) => {
            *pending_usage = Some(convert_usage(&usage));
            None
        }
        ChatEvent::TextDelta(text) => Some(ResponseEvent::OutputTextDelta(text)),
        ChatEvent::ReasoningDelta(delta) => Some(ResponseEvent::ReasoningContentDelta {
            delta,
            content_index: 0,
        }),
        ChatEvent::ToolCall(call) => {
            Some(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                id: None,
                name: call.name,
                namespace: None,
                arguments: call.arguments,
                encrypted_function_args: None,
                call_id: call.call_id,
                internal_chat_message_metadata_passthrough: None,
            }))
        }
        ChatEvent::Completed { finish } => Some(ResponseEvent::Completed {
            response_id: String::new(),
            token_usage: pending_usage.take(),
            usage_metadata: None,
            end_turn: Some(matches!(finish, FinishKind::Stop)),
        }),
    }
}

fn convert_usage(usage: &ChatTokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input as i64,
        cached_input_tokens: usage.cached_input as i64,
        cache_write_input_tokens: 0,
        output_tokens: usage.output as i64,
        reasoning_output_tokens: usage.reasoning_output as i64,
        total_tokens: usage.total as i64,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_wire::RepairOutcome;
    use crate::chat_wire::ToolCallDone;

    fn chat_usage(
        input: u64,
        output: u64,
        total: u64,
        cached: u64,
        reasoning: u64,
    ) -> ChatTokenUsage {
        ChatTokenUsage {
            input,
            output,
            total,
            cached_input: cached,
            reasoning_output: reasoning,
        }
    }

    #[test]
    fn maps_text_reasoning_and_tool_call_events() {
        let mut usage = None;

        let event = to_response_event(ChatEvent::TextDelta("hi".to_string()), &mut usage)
            .expect("text delta maps");
        assert!(matches!(event, ResponseEvent::OutputTextDelta(text) if text == "hi"));

        let event = to_response_event(ChatEvent::ReasoningDelta("hm".to_string()), &mut usage)
            .expect("reasoning delta maps");
        match event {
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => {
                assert_eq!(delta, "hm");
                assert_eq!(content_index, 0);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let event = to_response_event(
            ChatEvent::ToolCall(ToolCallDone {
                call_id: "c1".to_string(),
                name: "shell".to_string(),
                arguments: "{\"cmd\":\"ls\"}".to_string(),
                repair: RepairOutcome::Valid,
            }),
            &mut usage,
        )
        .expect("tool call maps");
        match event {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            }) => {
                assert_eq!(name, "shell");
                assert_eq!(arguments, "{\"cmd\":\"ls\"}");
                assert_eq!(call_id, "c1");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn usage_rides_on_completed() {
        let mut usage = None;
        assert!(
            to_response_event(ChatEvent::Usage(chat_usage(10, 5, 15, 2, 1)), &mut usage).is_none(),
            "usage is not a standalone event"
        );
        let event = to_response_event(
            ChatEvent::Completed {
                finish: FinishKind::Stop,
            },
            &mut usage,
        )
        .expect("completed maps");
        match event {
            ResponseEvent::Completed {
                token_usage,
                end_turn,
                ..
            } => {
                let usage = token_usage.expect("usage attached");
                assert_eq!(usage.input_tokens, 10);
                assert_eq!(usage.cached_input_tokens, 2);
                assert_eq!(usage.output_tokens, 5);
                assert_eq!(usage.reasoning_output_tokens, 1);
                assert_eq!(usage.total_tokens, 15);
                assert_eq!(end_turn, Some(true));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        // A Completed without fresh usage carries none, and tool-call turns
        // do not affirmatively end the turn.
        let event = to_response_event(
            ChatEvent::Completed {
                finish: FinishKind::ToolCalls,
            },
            &mut usage,
        )
        .expect("completed maps");
        assert!(matches!(
            event,
            ResponseEvent::Completed {
                token_usage: None,
                end_turn: Some(false),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn text_deltas_are_wrapped_in_item_events() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut text_state = TextItemState::default();
        let mut usage = None;
        assert!(
            emit_event(
                ChatEvent::TextDelta("he".to_string()),
                &mut text_state,
                &mut usage,
                &tx
            )
            .await
        );
        assert!(
            emit_event(
                ChatEvent::TextDelta("llo".to_string()),
                &mut text_state,
                &mut usage,
                &tx
            )
            .await
        );
        assert!(
            emit_event(
                ChatEvent::Completed {
                    finish: FinishKind::Stop
                },
                &mut text_state,
                &mut usage,
                &tx
            )
            .await
        );
        drop(tx);
        let mut events = Vec::new();
        while let Some(ev) = rx.recv().await {
            events.push(ev.expect("event"));
        }
        assert!(matches!(
            events[0],
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
        ));
        assert!(matches!(&events[1], ResponseEvent::OutputTextDelta(t) if t == "he"));
        assert!(matches!(&events[2], ResponseEvent::OutputTextDelta(t) if t == "llo"));
        match &events[3] {
            ResponseEvent::OutputItemDone(ResponseItem::Message { content, .. }) => {
                assert!(
                    matches!(&content[0], ContentItem::OutputText { text } if text == "hello")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(matches!(events[4], ResponseEvent::Completed { .. }));
    }

    #[test]
    fn convert_usage_fields() {
        let usage = convert_usage(&chat_usage(100, 40, 140, 60, 8));
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.cached_input_tokens, 60);
        assert_eq!(usage.cache_write_input_tokens, 0);
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(usage.reasoning_output_tokens, 8);
        assert_eq!(usage.total_tokens, 140);
    }
}
