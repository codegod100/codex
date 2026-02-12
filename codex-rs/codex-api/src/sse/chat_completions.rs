use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::error::ApiError;
use crate::rate_limits::parse_default_rate_limit;
use crate::telemetry::SseTelemetry;
use codex_client::ByteStream;
use codex_client::StreamResponse;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::timeout;
use tracing::debug;

#[derive(Debug, Default)]
struct ToolCallState {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    id: String,
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    delta: ChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatDeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ChatDeltaToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatDeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct ChatDeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
}

impl From<ChatUsage> for TokenUsage {
    fn from(usage: ChatUsage) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            cached_input_tokens: 0,
            output_tokens: usage.completion_tokens,
            reasoning_output_tokens: 0,
            total_tokens: usage.total_tokens,
        }
    }
}

pub fn spawn_chat_completions_stream(
    stream_response: StreamResponse,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) -> ResponseStream {
    let rate_limits = parse_default_rate_limit(&stream_response.headers);
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent, ApiError>>(1600);
    tokio::spawn(async move {
        if let Some(snapshot) = rate_limits {
            let _ = tx_event.send(Ok(ResponseEvent::RateLimits(snapshot))).await;
        }
        process_chat_completions_sse(stream_response.bytes, tx_event, idle_timeout, telemetry)
            .await;
    });

    ResponseStream { rx_event }
}

pub async fn process_chat_completions_sse(
    stream: ByteStream,
    tx_event: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn SseTelemetry>>,
) {
    let mut stream = stream.eventsource();
    let mut created_sent = false;
    let mut message_started = false;
    let mut message_text = String::new();
    let mut response_id: Option<String> = None;
    let mut usage: Option<TokenUsage> = None;
    let mut tool_calls: BTreeMap<usize, ToolCallState> = BTreeMap::new();

    loop {
        let wait_started = Instant::now();
        let poll = timeout(idle_timeout, stream.next()).await;
        if let Some(telemetry) = telemetry.as_ref() {
            telemetry.on_sse_poll(&poll, wait_started.elapsed());
        }

        let next_event = match poll {
            Ok(event) => event,
            Err(_) => {
                let _ = tx_event
                    .send(Err(ApiError::Retryable {
                        message: "chat completions stream idle timeout".to_string(),
                        delay: None,
                    }))
                    .await;
                return;
            }
        };

        let sse = match next_event {
            Some(Ok(sse)) => sse,
            Some(Err(err)) => {
                let _ = tx_event
                    .send(Err(ApiError::Stream(format!("SSE stream error: {err}"))))
                    .await;
                return;
            }
            None => break,
        };

        let data = sse.data.trim();
        if data == "[DONE]" {
            if message_started {
                let message = ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![ContentItem::OutputText {
                        text: message_text.clone(),
                    }],
                    end_turn: None,
                    phase: None,
                };
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(message)))
                    .await;
            }
            for (index, call) in tool_calls {
                let function_call = ResponseItem::FunctionCall {
                    id: None,
                    name: call.name.unwrap_or_else(|| "unknown".to_string()),
                    arguments: call.arguments,
                    call_id: call.id.unwrap_or_else(|| format!("chat_tool_call_{index}")),
                };
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputItemDone(function_call)))
                    .await;
            }
            let _ = tx_event
                .send(Ok(ResponseEvent::Completed {
                    response_id: response_id.unwrap_or_default(),
                    token_usage: usage,
                    can_append: false,
                }))
                .await;
            return;
        }

        let chunk: ChatCompletionChunk = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(err) => {
                debug!("failed to parse chat completion chunk: {err}");
                continue;
            }
        };
        if !created_sent {
            created_sent = true;
            let _ = tx_event.send(Ok(ResponseEvent::Created)).await;
        }
        response_id = Some(chunk.id.clone());
        usage = chunk.usage.map(Into::into);

        for choice in chunk.choices {
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                if !message_started {
                    message_started = true;
                    let started_item = ResponseItem::Message {
                        id: None,
                        role: "assistant".to_string(),
                        content: vec![ContentItem::OutputText {
                            text: String::new(),
                        }],
                        end_turn: None,
                        phase: None,
                    };
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemAdded(started_item)))
                        .await;
                }
                message_text.push_str(&content);
                let _ = tx_event
                    .send(Ok(ResponseEvent::OutputTextDelta(content)))
                    .await;
            }

            if let Some(delta_tool_calls) = choice.delta.tool_calls {
                for tool_call in delta_tool_calls {
                    let state = tool_calls.entry(tool_call.index).or_default();
                    if let Some(id) = tool_call.id {
                        state.id = Some(id);
                    }
                    if let Some(function) = tool_call.function {
                        if let Some(name) = function.name {
                            state.name = Some(name);
                        }
                        if let Some(arguments) = function.arguments {
                            state.arguments.push_str(&arguments);
                        }
                    }
                }
            }

            if let Some(finish_reason) = choice.finish_reason
                && finish_reason == "tool_calls"
            {
                for (index, call) in &tool_calls {
                    let function_call = ResponseItem::FunctionCall {
                        id: None,
                        name: call.name.clone().unwrap_or_else(|| "unknown".to_string()),
                        arguments: call.arguments.clone(),
                        call_id: call
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("chat_tool_call_{index}")),
                    };
                    let _ = tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(function_call)))
                        .await;
                }
            }
        }
    }

    let _ = tx_event
        .send(Err(ApiError::Stream(
            "chat completions stream disconnected before completion".to_string(),
        )))
        .await;
}
