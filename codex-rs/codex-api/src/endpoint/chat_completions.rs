use crate::auth::AuthProvider;
use crate::common::Prompt as ApiPrompt;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::sse::chat_completions::spawn_chat_completions_stream;
use crate::telemetry::SseTelemetry;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::instrument;

pub struct ChatCompletionsClient<T: HttpTransport, A: AuthProvider> {
    session: EndpointSession<T, A>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

pub struct ChatCompletionsOptions {
    pub extra_headers: HeaderMap,
    pub stream: bool,
}

impl Default for ChatCompletionsOptions {
    fn default() -> Self {
        Self {
            extra_headers: HeaderMap::new(),
            stream: true,
        }
    }
}

impl<T: HttpTransport, A: AuthProvider> ChatCompletionsClient<T, A> {
    pub fn new(transport: T, provider: Provider, auth: A) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
            sse_telemetry: None,
        }
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
            sse_telemetry: sse,
        }
    }

    #[instrument(level = "trace", skip_all, err)]
    pub async fn stream_prompt(
        &self,
        model: &str,
        prompt: &ApiPrompt,
        options: ChatCompletionsOptions,
    ) -> Result<ResponseStream, ApiError> {
        let body = build_chat_completions_request(model, prompt, options.stream);
        self.stream(body, options.extra_headers).await
    }

    fn path() -> &'static str {
        "chat/completions"
    }

    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        if body.get("stream").and_then(Value::as_bool) == Some(false) {
            let response = self
                .session
                .execute(Method::POST, Self::path(), extra_headers, Some(body))
                .await?;
            let body: Value = serde_json::from_slice(&response.body).map_err(|err| {
                ApiError::Stream(format!(
                    "failed to parse non-streaming chat completion response body: {err}"
                ))
            })?;
            return response_stream_from_non_stream_completion(body);
        }

        let stream_response = self
            .session
            .stream_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                },
            )
            .await?;

        Ok(spawn_chat_completions_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            self.sse_telemetry.clone(),
        ))
    }
}

fn build_chat_completions_request(model: &str, prompt: &ApiPrompt, stream: bool) -> Value {
    let mut body = json!({
        "model": model,
        "messages": build_chat_messages(prompt),
        "stream": stream,
    });
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }

    let tools = build_chat_tools(prompt);
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["parallel_tool_calls"] = Value::Bool(prompt.parallel_tool_calls);
        body["tool_choice"] = Value::String("auto".to_string());
    }

    body
}

fn build_chat_messages(prompt: &ApiPrompt) -> Vec<Value> {
    let mut messages = Vec::new();
    messages.push(json!({
        "role": "system",
        "content": prompt.instructions,
    }));

    for item in &prompt.input {
        match item {
            ResponseItem::Message { role, content, .. } => {
                if let Some(text) = extract_text_content(content)
                    && !text.is_empty()
                    && let Some(chat_role) = normalize_chat_role(role)
                {
                    messages.push(json!({
                        "role": chat_role,
                        "content": text,
                    }));
                }
            }
            ResponseItem::FunctionCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                messages.push(json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }]
                }));
            }
            ResponseItem::FunctionCallOutput { call_id, output } => {
                let output_text = match &output.body {
                    FunctionCallOutputBody::Text(text) => text.clone(),
                    FunctionCallOutputBody::ContentItems(_) => output.to_string(),
                };
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output_text,
                }));
            }
            _ => {}
        }
    }
    messages
}

fn normalize_chat_role(role: &str) -> Option<&str> {
    match role {
        // Several OpenAI-compatible Chat Completions providers reject
        // "developer"; map it to the closest supported role.
        "developer" => Some("system"),
        "system" | "user" | "assistant" => Some(role),
        _ => None,
    }
}

fn build_chat_tools(prompt: &ApiPrompt) -> Vec<Value> {
    let mut tools = Vec::new();
    for tool in &prompt.tools {
        if tool.get("type").and_then(|value| value.as_str()) == Some("function")
            && let Some(name) = tool.get("name").and_then(|value| value.as_str())
        {
            let mut function = json!({
                "name": name,
            });
            if let Some(description) = tool.get("description").and_then(|value| value.as_str()) {
                function["description"] = Value::String(description.to_string());
            }
            if let Some(parameters) = tool.get("parameters") {
                function["parameters"] = parameters.clone();
            }
            tools.push(json!({
                "type": "function",
                "function": function,
            }));
        }
    }
    tools
}

fn extract_text_content(content: &[ContentItem]) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => Some(text),
            ContentItem::InputImage { .. } => None,
        })
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");

    if text.is_empty() { None } else { Some(text) }
}

#[derive(Debug, Deserialize)]
struct NonStreamingCompletion {
    id: String,
    choices: Vec<NonStreamingChoice>,
    #[serde(default)]
    usage: Option<NonStreamingUsage>,
}

#[derive(Debug, Deserialize)]
struct NonStreamingChoice {
    message: NonStreamingMessage,
}

#[derive(Debug, Deserialize)]
struct NonStreamingMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<NonStreamingToolCall>>,
}

#[derive(Debug, Deserialize)]
struct NonStreamingToolCall {
    #[serde(default)]
    id: Option<String>,
    function: NonStreamingFunction,
}

#[derive(Debug, Deserialize)]
struct NonStreamingFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct NonStreamingUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
}

impl From<NonStreamingUsage> for TokenUsage {
    fn from(usage: NonStreamingUsage) -> Self {
        Self {
            input_tokens: usage.prompt_tokens,
            cached_input_tokens: 0,
            output_tokens: usage.completion_tokens,
            reasoning_output_tokens: 0,
            total_tokens: usage.total_tokens,
        }
    }
}

fn response_stream_from_non_stream_completion(body: Value) -> Result<ResponseStream, ApiError> {
    let completion: NonStreamingCompletion = serde_json::from_value(body).map_err(|err| {
        ApiError::Stream(format!(
            "failed to parse non-streaming chat completion: {err}"
        ))
    })?;
    let mut choices = completion.choices.into_iter();
    let message = choices.next().map(|choice| choice.message).ok_or_else(|| {
        ApiError::Stream("non-streaming chat completion had no choices".to_string())
    })?;
    let text = message.content.unwrap_or_default();
    let tool_calls = message.tool_calls.unwrap_or_default();

    let (tx_event, rx_event) = mpsc::channel::<Result<crate::common::ResponseEvent, ApiError>>(16);
    tokio::spawn(async move {
        let _ = tx_event
            .send(Ok(crate::common::ResponseEvent::Created))
            .await;
        if !text.is_empty() {
            let item = ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText { text }],
                end_turn: None,
                phase: None,
            };
            let _ = tx_event
                .send(Ok(crate::common::ResponseEvent::OutputItemDone(item)))
                .await;
        }
        for (index, tool_call) in tool_calls.into_iter().enumerate() {
            let item = ResponseItem::FunctionCall {
                id: None,
                name: tool_call.function.name,
                arguments: tool_call.function.arguments,
                call_id: tool_call
                    .id
                    .unwrap_or_else(|| format!("chat_tool_call_{index}")),
            };
            let _ = tx_event
                .send(Ok(crate::common::ResponseEvent::OutputItemDone(item)))
                .await;
        }
        let _ = tx_event
            .send(Ok(crate::common::ResponseEvent::Completed {
                response_id: completion.id,
                token_usage: completion.usage.map(Into::into),
            }))
            .await;
    });

    Ok(ResponseStream { rx_event })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Prompt as ApiPrompt;
    use crate::common::ResponseEvent;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use futures::StreamExt;
    use serde_json::json;

    #[tokio::test]
    async fn non_stream_completion_emits_function_calls() {
        let stream = response_stream_from_non_stream_completion(json!({
            "id": "resp_123",
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "exec_command",
                            "arguments": "{\"cmd\":\"ls\"}"
                        }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        }))
        .expect("non-stream body should parse");

        let events = stream
            .collect::<Vec<Result<ResponseEvent, ApiError>>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("stream should not error");

        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], ResponseEvent::Created));
        let ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            id,
            name,
            arguments,
            call_id,
        }) = &events[1]
        else {
            panic!("expected OutputItemDone(FunctionCall), got {:?}", events[1]);
        };
        assert_eq!(id, &None);
        assert_eq!(name, "exec_command");
        assert_eq!(arguments, "{\"cmd\":\"ls\"}");
        assert_eq!(call_id, "call_abc");
        let ResponseEvent::Completed {
            response_id,
            token_usage,
        } = &events[2]
        else {
            panic!("expected Completed, got {:?}", events[2]);
        };
        assert_eq!(response_id, "resp_123");
        assert_eq!(
            token_usage,
            &Some(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 5,
                reasoning_output_tokens: 0,
                total_tokens: 15
            })
        );
    }

    #[test]
    fn build_chat_messages_maps_developer_role_to_system() {
        let prompt = ApiPrompt {
            instructions: "instructions".to_string(),
            input: vec![
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "dev constraints".to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "hello".to_string(),
                    }],
                    end_turn: None,
                    phase: None,
                },
            ],
            tools: Vec::new(),
            parallel_tool_calls: false,
            output_schema: None,
        };

        let messages = build_chat_messages(&prompt);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], json!("system"));
        assert_eq!(messages[1]["content"], json!("dev constraints"));
        assert_eq!(messages[2]["role"], json!("user"));
    }
}
