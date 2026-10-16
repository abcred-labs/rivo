use crate::{
    ErrorKind, LlmClient, ProviderConfig, ProviderError, ProviderKind, ProviderRequest,
    ProviderResponse, ProviderStream, ProviderStreamEvent, ReasoningEffort, ResponseFormat,
    StopReason, ToolChoice, Usage,
};
use async_stream::try_stream;
use async_trait::async_trait;
use base64::Engine;
use futures::StreamExt;
use reqwest::{Client, Response, StatusCode};
use rivo_core::{ContentPart, ImageSource, MediaOperation, Message, Role};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const RIVO_USER_AGENT: &str = concat!("AbcRedAI/Rivo-Cli/", env!("RIVO_LLM_RELEASE_VERSION"));

fn build_http_client(timeout_seconds: u64) -> Result<Client, ProviderError> {
    Client::builder()
        .timeout(Duration::from_secs(timeout_seconds))
        .user_agent(RIVO_USER_AGENT)
        .build()
        .map_err(network_error)
}

#[derive(Clone)]
struct HttpProvider {
    kind: ProviderKind,
    config: ProviderConfig,
    client: Client,
}

#[derive(Clone)]
struct HttpMediaProvider {
    kind: ProviderKind,
    config: ProviderConfig,
    client: Client,
}

pub fn create_media_client(
    kind: ProviderKind,
    config: ProviderConfig,
) -> Result<std::sync::Arc<dyn crate::MediaClient>, ProviderError> {
    if !matches!(
        kind,
        ProviderKind::OpenAiChatCompletions | ProviderKind::OpenAiResponses
    ) {
        return Err(ProviderError::invalid(format!(
            "Provider {kind} 暂不支持图片生成或编辑"
        )));
    }
    let client = build_http_client(config.timeout_seconds)?;
    Ok(std::sync::Arc::new(HttpMediaProvider {
        kind,
        config,
        client,
    }))
}

#[async_trait]
impl crate::MediaClient for HttpMediaProvider {
    async fn execute_media(
        &self,
        request: crate::MediaProviderRequest,
    ) -> Result<crate::MediaProviderResponse, ProviderError> {
        if self.kind == ProviderKind::OpenAiResponses {
            return self.execute_responses_media(request).await;
        }
        self.execute_images_media(request).await
    }
}

impl HttpMediaProvider {
    async fn execute_images_media(
        &self,
        request: crate::MediaProviderRequest,
    ) -> Result<crate::MediaProviderResponse, ProviderError> {
        let base = self.config.base_url.trim_end_matches('/');
        let endpoint = match request.operation {
            MediaOperation::Generate => format!("{base}/images/generations"),
            MediaOperation::Edit => format!("{base}/images/edits"),
        };
        let response = match request.operation {
            MediaOperation::Generate => {
                let mut body = json!({
                    "model": request.model,
                    "prompt": request.prompt,
                    "n": 1,
                });
                if let Some(size) = request.size {
                    body["size"] = Value::String(size);
                }
                if let Some(output_media_type) = request.output_media_type.as_deref() {
                    body["output_format"] = Value::String(media_type_format(output_media_type)?);
                }
                let mut builder = self.client.post(endpoint).json(&body);
                if !self.config.api_key.is_empty() {
                    builder = builder.bearer_auth(&self.config.api_key);
                }
                builder.send().await.map_err(request_error)?
            }
            MediaOperation::Edit => {
                let input = request
                    .inputs
                    .first()
                    .ok_or_else(|| ProviderError::invalid("图像编辑至少需要一个输入媒体资产"))?;
                let filename = if input.media_type == "image/jpeg" {
                    "input.jpg"
                } else {
                    "input.png"
                };
                let image = reqwest::multipart::Part::bytes(input.data.clone())
                    .file_name(filename)
                    .mime_str(&input.media_type)
                    .map_err(|error| ProviderError::invalid(error.to_string()))?;
                let mut form = reqwest::multipart::Form::new()
                    .text("model", request.model)
                    .text("prompt", request.prompt)
                    // The Images edit endpoint documents the repeated image[]
                    // field, including for the single-image case.
                    .part("image[]", image);
                if let Some(size) = request.size {
                    form = form.text("size", size);
                }
                if let Some(output_media_type) = request.output_media_type.as_deref() {
                    form = form.text("output_format", media_type_format(output_media_type)?);
                }
                let mut builder = self.client.post(endpoint).multipart(form);
                if !self.config.api_key.is_empty() {
                    builder = builder.bearer_auth(&self.config.api_key);
                }
                builder.send().await.map_err(request_error)?
            }
        };
        if !response.status().is_success() {
            return Err(http_error(response, &self.config.api_key).await);
        }
        let value: Value = response.json().await.map_err(network_error)?;
        let encoded = value
            .pointer("/data/0/b64_json")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::invalid_response("图片响应缺少 data[0].b64_json"))?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| {
                ProviderError::invalid_response(format!("图片 Base64 解码失败: {error}"))
            })?;
        Ok(crate::MediaProviderResponse {
            data,
            media_type: request
                .output_media_type
                .unwrap_or_else(|| "image/png".into()),
        })
    }

    async fn execute_responses_media(
        &self,
        request: crate::MediaProviderRequest,
    ) -> Result<crate::MediaProviderResponse, ProviderError> {
        let base = self.config.base_url.trim_end_matches('/');
        let endpoint = format!("{base}/responses");
        let mut image_tool = json!({"type": "image_generation"});
        if let Some(size) = request.size.as_deref() {
            image_tool["size"] = Value::String(size.into());
        }
        if let Some(output_media_type) = request.output_media_type.as_deref() {
            image_tool["output_format"] = Value::String(media_type_format(output_media_type)?);
        }

        let input = if request.inputs.is_empty() {
            Value::String(request.prompt.clone())
        } else {
            let mut content = vec![json!({
                "type": "input_text",
                "text": request.prompt,
            })];
            for input in request.inputs {
                let encoded = base64::engine::general_purpose::STANDARD.encode(input.data);
                content.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{};base64,{encoded}", input.media_type),
                }));
            }
            json!([{"role":"user","content":content}])
        };
        let body = json!({
            "model": request.model,
            "input": input,
            "tools": [image_tool],
        });
        let mut builder = self.client.post(endpoint).json(&body);
        if !self.config.api_key.is_empty() {
            builder = builder.bearer_auth(&self.config.api_key);
        }
        let response = builder.send().await.map_err(request_error)?;
        if !response.status().is_success() {
            return Err(http_error(response, &self.config.api_key).await);
        }
        let value: Value = response.json().await.map_err(network_error)?;
        let encoded = value
            .get("output")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find_map(|item| {
                    (item.get("type").and_then(Value::as_str) == Some("image_generation_call"))
                        .then(|| item.get("result"))
                        .flatten()
                        .and_then(Value::as_str)
                })
            })
            .ok_or_else(|| {
                ProviderError::invalid_response(
                    "Responses 图片响应缺少 output[].image_generation_call.result",
                )
            })?;
        let data = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| {
                ProviderError::invalid_response(format!("图片 Base64 解码失败: {error}"))
            })?;
        Ok(crate::MediaProviderResponse {
            data,
            media_type: request
                .output_media_type
                .unwrap_or_else(|| "image/png".into()),
        })
    }
}

fn media_type_format(media_type: &str) -> Result<String, ProviderError> {
    match media_type {
        "image/png" => Ok("png".into()),
        "image/jpeg" => Ok("jpeg".into()),
        "image/webp" => Ok("webp".into()),
        _ => Err(ProviderError::invalid(format!(
            "不支持的图片输出格式: {media_type}"
        ))),
    }
}

impl HttpProvider {
    fn new(kind: ProviderKind, config: ProviderConfig) -> Result<Self, ProviderError> {
        let client = build_http_client(config.timeout_seconds)?;
        Ok(Self {
            kind,
            config,
            client,
        })
    }

    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let response = self.send(request.clone(), false).await?;
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let value: Value = response.json().await.map_err(network_error)?;
        normalize_response(self.kind, value, request_id, &request)
    }

    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let url = models_url(self.kind, &self.config.base_url);
        let mut request = self.client.get(url);
        if self.config.api_key.is_empty() {
            if self.kind == ProviderKind::ClaudeMessages {
                request = request.header("anthropic-version", "2023-06-01");
            }
        } else {
            request = match self.kind {
                ProviderKind::ClaudeMessages => request
                    .header("x-api-key", &self.config.api_key)
                    .header("anthropic-version", "2023-06-01"),
                _ => request.bearer_auth(&self.config.api_key),
            };
        }
        let response = request.send().await.map_err(request_error)?;
        if !response.status().is_success() {
            return Err(http_error(response, &self.config.api_key).await);
        }
        let value: Value = response.json().await.map_err(network_error)?;
        parse_model_list(self.kind, value)
    }
}

fn models_url(_kind: ProviderKind, base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

/// Builds the provider request URL from an endpoint prefix and model. Claude
/// Messages accepts both its API root and an already-versioned `/v1` prefix.
pub fn provider_request_url(kind: ProviderKind, base_url: &str, _model: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let endpoint = match kind {
        ProviderKind::OpenAiChatCompletions => "chat/completions",
        ProviderKind::OpenAiResponses => "responses",
        ProviderKind::ClaudeMessages if base.rsplit('/').next() == Some("v1") => "messages",
        ProviderKind::ClaudeMessages => "v1/messages",
    };
    format!("{base}/{endpoint}")
}

fn parse_model_list(_kind: ProviderKind, value: Value) -> Result<Vec<String>, ProviderError> {
    let mut models = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::invalid_response("/v1/models 响应缺少 data 数组"))?
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    models.retain(|id| seen.insert(id.clone()));
    Ok(models)
}

impl HttpProvider {
    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        let this = self.clone();
        Box::pin(try_stream! {
            let response = this.send(request, true).await?;
            let mut chunks = response.bytes_stream();
            let mut buffer = Vec::new();
            let mut data = Vec::new();
            let mut accumulator = StreamAccumulator::new(this.kind);
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk.map_err(network_error)?;
                for payload in parse_sse(&mut buffer, &mut data, &chunk, this.config.max_stream_event_bytes)? {
                    if payload == "[DONE]" { if let Some(event) = accumulator.finish() { yield event; } continue; }
                    let value: Value = serde_json::from_str(&payload).map_err(|error| ProviderError::invalid_response(format!("无效 SSE JSON: {error}")))?;
                    for event in accumulator.normalize(&value)? { yield event; }
                }
            }
            for payload in finish_sse(&mut buffer, &mut data, this.config.max_stream_event_bytes)? {
                if payload == "[DONE]" { if let Some(event) = accumulator.finish() { yield event; } continue; }
                let value: Value = serde_json::from_str(&payload).map_err(|error| ProviderError::invalid_response(format!("无效 SSE JSON: {error}")))?;
                for event in accumulator.normalize(&value)? { yield event; }
            }
            if !accumulator.done { Err(ProviderError::invalid_response("SSE 流在协议终态前结束"))?; }
        })
    }

    async fn send(
        &self,
        request: ProviderRequest,
        stream: bool,
    ) -> Result<Response, ProviderError> {
        let url = provider_request_url(self.kind, &self.config.base_url, &request.model);
        let body = build_request(self.kind, &request, stream)?;
        let mut builder = self.client.post(url).json(&body);
        if self.config.api_key.is_empty() {
            if self.kind == ProviderKind::ClaudeMessages {
                builder = builder.header("anthropic-version", "2023-06-01");
            }
        } else {
            builder = match self.kind {
                ProviderKind::ClaudeMessages => builder
                    .header("x-api-key", &self.config.api_key)
                    .header("anthropic-version", "2023-06-01"),
                _ => builder.bearer_auth(&self.config.api_key),
            };
        }
        let mut last_error = None;
        for attempt in 0..3 {
            let response = builder
                .try_clone()
                .ok_or_else(|| ProviderError::invalid("LLM 请求无法安全重试"))?
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.trim().parse::<u64>().ok())
                        .map(|seconds| Duration::from_secs(seconds.min(5)));
                    let error = http_error(response, &self.config.api_key).await;
                    if !error.retriable || attempt == 2 {
                        return Err(error);
                    }
                    last_error = Some(error);
                    tokio::time::sleep(
                        retry_after.unwrap_or_else(|| Duration::from_millis(100 * (1 << attempt))),
                    )
                    .await;
                    continue;
                }
                Err(error) => {
                    let error = request_error(error);
                    if !error.retriable || attempt == 2 {
                        return Err(error);
                    }
                    last_error = Some(error);
                }
            }
            tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
        }
        Err(last_error.unwrap_or_else(|| ProviderError::invalid("LLM 请求未执行")))
    }
}

/// Provider 不携带协议，因此 discovery 先用 Bearer 认证；仅当端点以 401/403
/// 拒绝时，再按 Anthropic 风格（`x-api-key` + `anthropic-version`）重试一次。
pub async fn discover_models(config: ProviderConfig) -> Result<Vec<String>, ProviderError> {
    let bearer = HttpProvider::new(ProviderKind::OpenAiChatCompletions, config.clone())?
        .list_models()
        .await;
    match bearer {
        // 重试仍失败时返回首次的认证错误，保留明确的 401/403 诊断。
        Err(error) if matches!(error.http_status, Some(401 | 403)) => {
            HttpProvider::new(ProviderKind::ClaudeMessages, config)?
                .list_models()
                .await
                .map_err(|_| error)
        }
        result => result,
    }
}

macro_rules! provider_client {
    ($name:ident, $kind:expr) => {
        #[derive(Clone)]
        pub struct $name(HttpProvider);

        impl $name {
            pub fn new(config: ProviderConfig) -> Result<Self, ProviderError> {
                Ok(Self(HttpProvider::new($kind, config)?))
            }
        }

        #[async_trait]
        impl LlmClient for $name {
            async fn complete(
                &self,
                request: ProviderRequest,
            ) -> Result<ProviderResponse, ProviderError> {
                self.0.complete(request).await
            }

            fn stream(&self, request: ProviderRequest) -> ProviderStream {
                self.0.stream(request)
            }
        }
    };
}

provider_client!(
    OpenAiChatCompletionsClient,
    ProviderKind::OpenAiChatCompletions
);
provider_client!(OpenAiResponsesClient, ProviderKind::OpenAiResponses);
provider_client!(ClaudeMessagesClient, ProviderKind::ClaudeMessages);

fn build_request(
    kind: ProviderKind,
    request: &ProviderRequest,
    stream: bool,
) -> Result<Value, ProviderError> {
    match kind {
        ProviderKind::OpenAiChatCompletions => build_chat_request(request, stream),
        ProviderKind::OpenAiResponses => build_responses_request(request, stream),
        ProviderKind::ClaudeMessages => build_claude_messages_request(request, stream),
    }
}

fn build_chat_request(request: &ProviderRequest, stream: bool) -> Result<Value, ProviderError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        messages.extend(chat_messages(message)?);
    }
    body.insert("messages".into(), Value::Array(messages));
    insert_common_openai_fields(&mut body, request, stream, false);
    Ok(Value::Object(body))
}

fn build_responses_request(
    request: &ProviderRequest,
    stream: bool,
) -> Result<Value, ProviderError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    if let Some(system) = &request.system {
        body.insert("instructions".into(), json!(system));
    }
    body.insert(
        "input".into(),
        Value::Array(
            request
                .messages
                .iter()
                .map(|message| {
                    responses_messages(message, request.computer_use_function_fallback())
                })
                .collect::<Result<Vec<Vec<_>>, _>>()?
                .into_iter()
                .flatten()
                .collect(),
        ),
    );
    insert_common_openai_fields(&mut body, request, stream, true);
    Ok(Value::Object(body))
}

fn build_claude_messages_request(
    request: &ProviderRequest,
    stream: bool,
) -> Result<Value, ProviderError> {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert(
        "max_tokens".into(),
        json!(request.max_output_tokens.unwrap_or(1024)),
    );
    body.insert("stream".into(), json!(stream));
    if let Some(system) = &request.system {
        // Keep the stable system prompt cacheable while leaving dynamic
        // conversation content outside the cached prefix.
        body.insert(
            "system".into(),
            json!([{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"}
            }]),
        );
    }
    body.insert(
        "messages".into(),
        Value::Array(
            request
                .messages
                .iter()
                .filter(|message| message.role != Role::System)
                .map(claude_messages_message)
                .collect::<Result<Vec<_>, _>>()?,
        ),
    );
    if !request.tools.is_empty() || request.computer_use_enabled() {
        let mut tools = request
            .tools
            .iter()
            .filter(|tool| !(request.computer_use_enabled() && tool.name == "computer"))
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect::<Vec<_>>();
        if request.computer_use_enabled() {
            tools.push(crate::computer_use::tool_definition(
                ProviderKind::ClaudeMessages,
            ));
        }
        body.insert("tools".into(), Value::Array(tools));
        body.insert(
            "tool_choice".into(),
            claude_messages_tool_choice(&request.tool_choice),
        );
    }
    if !matches!(request.reasoning_effort, ReasoningEffort::ProviderDefault) {
        let budget_tokens = match request.reasoning_effort {
            ReasoningEffort::Low => 1024,
            ReasoningEffort::Medium => 4096,
            ReasoningEffort::High => 8192,
            ReasoningEffort::ProviderDefault => unreachable!(),
        };
        body.insert(
            "thinking".into(),
            json!({"type": "enabled", "budget_tokens": budget_tokens}),
        );
    }
    // Claude-compatible gateways increasingly reject the legacy temperature
    // field for newer models; sampling defaults remain provider-controlled.
    if let Some(format) = &request.response_format {
        let (name, schema) = match format {
            ResponseFormat::JsonSchema { name, schema, .. } => (name.clone(), schema.clone()),
            ResponseFormat::JsonObject => ("rivo_json_result".into(), json!({"type":"object"})),
        };
        if request.tools.iter().any(|tool| tool.name == name) {
            return Err(ProviderError::invalid(format!(
                "结构化结果工具名冲突: {name}"
            )));
        }
        let mut tools = body
            .remove("tools")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        tools.push(json!({
            "name": name,
            "description": "返回最终结构化结果",
            "input_schema": schema,
        }));
        body.insert("tools".into(), Value::Array(tools));
        body.insert("tool_choice".into(), json!({"type": "tool", "name": name}));
    }
    Ok(Value::Object(body))
}

fn insert_common_openai_fields(
    body: &mut Map<String, Value>,
    request: &ProviderRequest,
    stream: bool,
    responses: bool,
) {
    body.insert("stream".into(), json!(stream));
    if stream && !responses {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if !request.tools.is_empty() || (responses && request.computer_use_enabled()) {
        let mut tools = request
            .tools
            .iter()
            .filter(|tool| !(responses && request.computer_use_enabled() && tool.name == "computer"))
            .map(|tool| {
                if responses {
                    json!({"type":"function", "name":tool.name, "description":tool.description, "parameters":tool.parameters})
                } else {
                    json!({"type":"function", "function":{"name":tool.name, "description":tool.description, "parameters":tool.parameters}})
                }
            })
            .collect::<Vec<_>>();
        if responses && request.computer_use_enabled() {
            tools.push(if request.computer_use_function_fallback() {
                crate::computer_use::function_tool_definition()
            } else {
                crate::computer_use::tool_definition(ProviderKind::OpenAiResponses)
            });
        }
        body.insert("tools".into(), Value::Array(tools));
        body.insert(
            "tool_choice".into(),
            openai_tool_choice(&request.tool_choice, responses),
        );
    }
    if let Some(max_tokens) = request.max_output_tokens {
        body.insert(
            if responses {
                "max_output_tokens".into()
            } else {
                "max_tokens".into()
            },
            json!(max_tokens),
        );
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if !matches!(request.reasoning_effort, ReasoningEffort::ProviderDefault) {
        body.insert(
            if responses {
                "reasoning"
            } else {
                "reasoning_effort"
            }
            .into(),
            if responses {
                json!({"effort": request.reasoning_effort.as_str()})
            } else {
                json!(request.reasoning_effort.as_str())
            },
        );
    }
    if let Some(format) = &request.response_format {
        let value = openai_response_format(format, responses);
        if responses {
            body.insert("text".into(), json!({"format": value}));
        } else {
            body.insert("response_format".into(), value);
        }
    }
}

fn chat_messages(message: &Message) -> Result<Vec<Value>, ProviderError> {
    let role = role_name(message.role);
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    let mut tool_results = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => content.push(json!({"type":"text", "text":text})),
            ContentPart::Image { source } => content.push(chat_image(source)),
            ContentPart::ToolUse {
                id,
                name,
                arguments,
            } => tool_calls.push(json!({
                "id":id,
                "type":"function",
                "function":{"name":name, "arguments":arguments.to_string()}
            })),
            ContentPart::ToolResult {
                tool_call_id: id,
                content: result,
                is_error,
            } => {
                if let Some(image) = result.iter().find_map(image_content) {
                    tool_results.push(json!({
                        "role":"tool", "tool_call_id":id,
                        "content":[{"type":"image_url", "image_url":{"url":image_url(&image)}}]
                    }));
                } else {
                    tool_results.push(json!({
                        "role":"tool",
                        "tool_call_id":id,
                        "content":tool_result_text(result, *is_error)
                    }));
                }
            }
        }
    }
    let mut value = json!({"role":role, "content":content});
    if !tool_calls.is_empty() {
        value["tool_calls"] = Value::Array(tool_calls);
    }
    let has_tool_calls = value
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| !calls.is_empty())
        .unwrap_or(false);
    let mut values = Vec::new();
    if !content.is_empty() || has_tool_calls {
        values.push(value);
    }
    values.extend(tool_results);
    Ok(values)
}

fn responses_messages(
    message: &Message,
    computer_function_fallback: bool,
) -> Result<Vec<Value>, ProviderError> {
    let mut items = Vec::new();
    let mut content = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => content.push(json!({
                "type": if message.role == Role::Assistant {"output_text"} else {"input_text"},
                "text": text
            })),
            ContentPart::Image { source } => content.push(responses_image(source)),
            ContentPart::ToolUse {
                id,
                name,
                arguments,
            } => {
                items.push(json!({
                    "type":"function_call", "call_id":id, "name":name, "arguments":arguments.to_string()
                }));
            }
            ContentPart::ToolResult {
                tool_call_id,
                content: result,
                is_error,
            } => {
                if computer_function_fallback {
                    items.push(json!({
                        "type":"function_call_output", "call_id":tool_call_id,
                        "output":tool_result_text(result, *is_error)
                    }));
                } else if let Some(image) = result.iter().find_map(image_content) {
                    items.push(json!({
                        "type":"computer_call_output", "call_id":tool_call_id,
                        "output":{"type":"computer_screenshot", "image_url":image_url(&image), "detail":"original"}
                    }));
                } else {
                    items.push(json!({
                        "type":"function_call_output", "call_id":tool_call_id,
                        "output":tool_result_text(result, *is_error)
                    }));
                }
            }
        }
    }
    if !content.is_empty() {
        items.insert(
            0,
            json!({"role": role_name(message.role), "content": content}),
        );
    }
    Ok(items)
}

fn claude_messages_message(message: &Message) -> Result<Value, ProviderError> {
    let role = if message.role == Role::Assistant {
        "assistant"
    } else {
        "user"
    };
    let mut content = Vec::new();
    for part in &message.content {
        content.push(match part {
            ContentPart::Text { text } => json!({"type":"text", "text":text}),
            ContentPart::Image { source } => claude_messages_image(source),
            ContentPart::ToolUse {
                id,
                name,
                arguments,
            } => {
                json!({"type":"tool_use", "id":id, "name":name, "input":arguments})
            }
            ContentPart::ToolResult {
                tool_call_id,
                content: result,
                is_error,
            } => {
                let content = if let Some(image) = result.iter().find_map(image_content) {
                    vec![claude_messages_image(&image)]
                } else {
                    vec![json!({"type":"text", "text":tool_result_text(result, *is_error)})]
                };
                json!({
                    "type":"tool_result", "tool_use_id":tool_call_id,
                    "content":content, "is_error":is_error
                })
            }
        });
    }
    Ok(json!({"role":role, "content":content}))
}

fn normalize_response(
    kind: ProviderKind,
    value: Value,
    request_id: Option<String>,
    request: &ProviderRequest,
) -> Result<ProviderResponse, ProviderError> {
    match kind {
        ProviderKind::OpenAiChatCompletions => normalize_chat_response(value, request_id, request),
        ProviderKind::OpenAiResponses => normalize_responses_response(value, request_id, request),
        ProviderKind::ClaudeMessages => {
            normalize_claude_messages_response(value, request_id, request)
        }
    }
}

fn normalize_chat_response(
    value: Value,
    request_id: Option<String>,
    request: &ProviderRequest,
) -> Result<ProviderResponse, ProviderError> {
    let choice = value
        .pointer("/choices/0")
        .ok_or_else(|| ProviderError::invalid("Chat Completions 响应缺少 choices[0]"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| ProviderError::invalid("Chat Completions 响应缺少 message"))?;
    let mut parts = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            parts.push(ContentPart::Text { text: text.into() });
        }
    }
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            parts.push(ContentPart::ToolUse {
                id: string_at(call, "/id")?,
                name: string_at(call, "/function/name")?,
                arguments: parse_arguments(call.pointer("/function/arguments"))?,
            });
        }
    }
    let finish = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop");
    Ok(ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: parts,
        },
        structured_output: structured_output(&value, request)?,
        stop_reason: match finish {
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            "content_filter" => StopReason::ContentFilter,
            _ => StopReason::Final,
        },
        usage: openai_usage(value.get("usage")),
        provider_raw: Some(value.clone()),
        request_id,
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .into(),
    })
}

fn normalize_responses_response(
    value: Value,
    request_id: Option<String>,
    request: &ProviderRequest,
) -> Result<ProviderResponse, ProviderError> {
    let mut parts = Vec::new();
    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for content in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if matches!(
                        content.get("type").and_then(Value::as_str),
                        Some("output_text")
                    ) {
                        if let Some(text) = content.get("text").and_then(Value::as_str) {
                            parts.push(ContentPart::Text { text: text.into() });
                        }
                    }
                }
            }
            Some("function_call") => parts.push(ContentPart::ToolUse {
                id: string_at(item, "/call_id")?,
                name: string_at(item, "/name")?,
                arguments: parse_arguments(item.get("arguments"))?,
            }),
            Some("computer_call") => parts.push(ContentPart::ToolUse {
                id: item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ProviderError::invalid_response("Responses computer_call 缺少 call_id")
                    })?
                    .into(),
                name: "computer".into(),
                arguments: json!({
                    "actions": item.get("actions").cloned().ok_or_else(|| {
                        ProviderError::invalid_response("Responses computer_call 缺少 actions")
                    })?
                }),
            }),
            _ => {}
        }
    }
    let stop_reason = if parts
        .iter()
        .any(|part| matches!(part, ContentPart::ToolUse { .. }))
    {
        StopReason::ToolUse
    } else if value.get("status").and_then(Value::as_str) == Some("incomplete") {
        match value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::ContentFilter,
            _ => StopReason::Error,
        }
    } else {
        StopReason::Final
    };
    Ok(ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: parts,
        },
        structured_output: structured_output(&value, request)?,
        stop_reason,
        usage: responses_usage(value.get("usage")),
        provider_raw: Some(value.clone()),
        request_id: request_id.or_else(|| value.get("id").and_then(Value::as_str).map(Into::into)),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .into(),
    })
}

fn normalize_claude_messages_response(
    value: Value,
    request_id: Option<String>,
    request: &ProviderRequest,
) -> Result<ProviderResponse, ProviderError> {
    let mut parts = Vec::new();
    let result_tool = match &request.response_format {
        Some(ResponseFormat::JsonSchema { name, .. }) => Some(name.as_str()),
        Some(ResponseFormat::JsonObject) => Some("rivo_json_result"),
        _ => None,
    };
    let mut normalized_structured_output = None;
    for content in value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match content.get("type").and_then(Value::as_str) {
            Some("text") => parts.push(ContentPart::Text {
                text: string_at(content, "/text")?,
            }),
            Some("tool_use") => {
                let name = string_at(content, "/name")?;
                let arguments = content.get("input").cloned().unwrap_or_else(|| json!({}));
                if result_tool == Some(name.as_str()) {
                    normalized_structured_output = Some(arguments.clone());
                    parts.push(ContentPart::Text {
                        text: arguments.to_string(),
                    });
                } else {
                    parts.push(ContentPart::ToolUse {
                        id: string_at(content, "/id")?,
                        name,
                        arguments,
                    });
                }
            }
            _ => {}
        }
    }
    // Some Claude-compatible gateways ignore the
    // forced result tool and return the requested JSON as plain text. Accept
    // that form only when the request explicitly asked for structured output
    // and the text parses as JSON; ordinary text responses still fail closed.
    if request.response_format.is_some() && normalized_structured_output.is_none() {
        let tool_values = parts.iter().filter_map(|part| match part {
            ContentPart::ToolUse { arguments, .. } => Some(arguments),
            _ => None,
        });
        let mut tool_values = tool_values;
        if let (Some(value), None) = (tool_values.next(), tool_values.next()) {
            normalized_structured_output = Some(value.clone());
        }
    }
    if request.response_format.is_some() && normalized_structured_output.is_none() {
        if let Some(text) = parts.iter().find_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        }) {
            if let Ok(value) = serde_json::from_str(text) {
                normalized_structured_output = Some(value);
            }
        }
    }
    if request.response_format.is_some() && normalized_structured_output.is_none() {
        return Err(ProviderError::invalid_response(
            "Claude Messages 响应缺少结构化结果工具调用",
        ));
    }
    let stop_reason = if normalized_structured_output.is_some() {
        StopReason::Final
    } else {
        match value.get("stop_reason").and_then(Value::as_str) {
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            _ => StopReason::Final,
        }
    };
    Ok(ProviderResponse {
        message: Message {
            role: Role::Assistant,
            content: parts,
        },
        structured_output: normalized_structured_output,
        stop_reason,
        usage: claude_messages_usage(value.get("usage")),
        provider_raw: Some(value.clone()),
        request_id: request_id.or_else(|| value.get("id").and_then(Value::as_str).map(Into::into)),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .into(),
    })
}

fn parse_sse(
    buffer: &mut Vec<u8>,
    data: &mut Vec<u8>,
    chunk: &[u8],
    max_event_bytes: usize,
) -> Result<Vec<String>, ProviderError> {
    buffer.extend_from_slice(chunk);
    if buffer.len() > max_event_bytes && !buffer.contains(&b'\n') {
        return Err(ProviderError::invalid_response("SSE 行超过大小限制"));
    }
    let mut payloads = Vec::new();
    while let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
        let line = buffer.drain(..=index).collect::<Vec<_>>();
        let line = &line[..line.len().saturating_sub(1)];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        process_sse_line(line, data, max_event_bytes, &mut payloads)?;
    }
    if buffer.len() > max_event_bytes {
        return Err(ProviderError::invalid_response("SSE 缓冲超过大小限制"));
    }
    Ok(payloads)
}

fn finish_sse(
    buffer: &mut Vec<u8>,
    data: &mut Vec<u8>,
    max_event_bytes: usize,
) -> Result<Vec<String>, ProviderError> {
    let mut payloads = Vec::new();
    if !buffer.is_empty() {
        let line = std::mem::take(buffer);
        let line = line.strip_suffix(b"\r").unwrap_or(&line);
        process_sse_line(line, data, max_event_bytes, &mut payloads)?;
    }
    if !data.is_empty() {
        let payload = String::from_utf8(std::mem::take(data))
            .map_err(|_| ProviderError::invalid_response("SSE data 不是有效 UTF-8"))?;
        payloads.push(payload);
    }
    Ok(payloads)
}

fn process_sse_line(
    line: &[u8],
    data: &mut Vec<u8>,
    max_event_bytes: usize,
    payloads: &mut Vec<String>,
) -> Result<(), ProviderError> {
    if line.is_empty() {
        if !data.is_empty() {
            let payload = String::from_utf8(std::mem::take(data))
                .map_err(|_| ProviderError::invalid_response("SSE data 不是有效 UTF-8"))?;
            payloads.push(payload);
        }
        return Ok(());
    }
    let line = std::str::from_utf8(line)
        .map_err(|_| ProviderError::invalid_response("SSE 行不是有效 UTF-8"))?;
    let Some(value) = line.strip_prefix("data:") else {
        return Ok(());
    };
    let value = value.strip_prefix(' ').unwrap_or(value);
    if !data.is_empty() {
        data.push(b'\n');
    }
    data.extend_from_slice(value.as_bytes());
    if data.len() > max_event_bytes {
        return Err(ProviderError::invalid_response("SSE event 超过大小限制"));
    }
    Ok(())
}

struct ToolIdentity {
    call_id: String,
    name: Option<String>,
    /// Whether any argument text has been forwarded for this call. Some OpenAI Responses-compatible
    /// providers (observed with strict tool schemas) send no `function_call_arguments.delta` and only
    /// the complete arguments in the `.done` events; those must still reach the agent exactly once.
    streamed: bool,
}

struct StreamAccumulator {
    kind: ProviderKind,
    tools: HashMap<String, ToolIdentity>,
    /// Responses only: every identifier a gateway may use for one function call (`item_id`,
    /// `call_id`, `#<output_index>`) maps to the canonical key in `tools`. Parallel calls are
    /// interleaved and gateways are inconsistent about which identifier each event carries.
    aliases: HashMap<String, String>,
    /// Last resolved Responses call, for events that carry no identifier at all.
    last_call: Option<String>,
    text_seen: bool,
    usage: Usage,
    stop_reason: Option<StopReason>,
    done: bool,
}

/// Maps an in-band Responses `error` / `response.failed` event to a provider error that keeps the
/// gateway's own message instead of a generic "stream ended early".
fn responses_stream_error(value: &Value) -> ProviderError {
    let error = value
        .pointer("/response/error")
        .or_else(|| value.get("error"))
        .unwrap_or(value);
    let code = error
        .get("code")
        .or_else(|| error.get("type"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Responses stream reported a failure");
    let lowered = code.as_deref().unwrap_or_default().to_ascii_lowercase();
    let (kind, retriable) = if lowered.contains("rate_limit") {
        (ErrorKind::RateLimit, true)
    } else if lowered.contains("context_length") {
        (ErrorKind::ContextLength, false)
    } else if lowered.contains("invalid") {
        (ErrorKind::InvalidRequest, false)
    } else {
        (ErrorKind::Server, true)
    };
    ProviderError {
        kind,
        retriable,
        http_status: None,
        provider_code: code,
        message: format!("Responses stream error: {}", truncate_message(message)),
    }
}

fn truncate_message(message: &str) -> String {
    message.chars().take(512).collect()
}

impl StreamAccumulator {
    fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            tools: HashMap::new(),
            aliases: HashMap::new(),
            last_call: None,
            text_seen: false,
            usage: Usage::default(),
            stop_reason: None,
            done: false,
        }
    }

    fn normalize(&mut self, value: &Value) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        if self.done {
            return Ok(Vec::new());
        }
        match self.kind {
            ProviderKind::OpenAiChatCompletions => self.chat(value),
            ProviderKind::OpenAiResponses => self.responses(value),
            ProviderKind::ClaudeMessages => self.claude_messages(value),
        }
    }

    fn chat(&mut self, value: &Value) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        let mut events = Vec::new();
        if let Some(delta) = value
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        {
            events.push(ProviderStreamEvent::TextDelta {
                delta: delta.into(),
            });
        }
        for call in value
            .pointer("/choices/0/delta/tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let index = call
                .get("index")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .to_string();
            let identity = self
                .tools
                .entry(index.clone())
                .or_insert_with(|| ToolIdentity {
                    call_id: format!("chat-call-{index}"),
                    name: None,
                    streamed: true,
                });
            if let Some(id) = call.get("id").and_then(Value::as_str) {
                identity.call_id = id.into();
            }
            if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                identity.name = Some(name.into());
            }
            events.push(ProviderStreamEvent::ToolArgumentsDelta {
                call_id: identity.call_id.clone(),
                name: identity.name.clone(),
                delta: call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
            });
        }
        if let Some(usage) = value.get("usage") {
            merge_usage(&mut self.usage, openai_usage(Some(usage)));
        }
        if let Some(finish) = value
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            self.stop_reason = Some(match finish {
                "tool_calls" => StopReason::ToolUse,
                "length" => StopReason::MaxTokens,
                "content_filter" => StopReason::ContentFilter,
                "stop" => StopReason::Final,
                _ => StopReason::Error,
            });
        }
        Ok(events)
    }

    fn responses(&mut self, value: &Value) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        let mut events = Vec::new();
        let output_index = value.get("output_index").and_then(Value::as_u64);
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                let delta = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.text_seen |= !delta.is_empty();
                events.push(ProviderStreamEvent::TextDelta {
                    delta: delta.into(),
                });
            }
            Some("response.output_item.added") => {
                if let Some(item) = value.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        self.responses_identity(
                            item.get("id").and_then(Value::as_str),
                            item.get("call_id").and_then(Value::as_str),
                            output_index,
                            item.get("name").and_then(Value::as_str),
                        );
                    }
                }
            }
            Some("response.function_call_arguments.delta") => {
                let key = self.responses_identity(
                    value.get("item_id").and_then(Value::as_str),
                    value.get("call_id").and_then(Value::as_str),
                    output_index,
                    value.get("name").and_then(Value::as_str),
                );
                let identity = self
                    .tools
                    .get_mut(&key)
                    .expect("identity was just resolved");
                identity.streamed = true;
                events.push(ProviderStreamEvent::ToolArgumentsDelta {
                    call_id: identity.call_id.clone(),
                    name: identity.name.clone(),
                    delta: value
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .into(),
                });
            }
            Some("response.function_call_arguments.done") => {
                let key = self.responses_identity(
                    value.get("item_id").and_then(Value::as_str),
                    value.get("call_id").and_then(Value::as_str),
                    output_index,
                    value.get("name").and_then(Value::as_str),
                );
                self.complete_responses_call(
                    &key,
                    value.get("arguments").and_then(Value::as_str),
                    &mut events,
                );
            }
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        self.complete_responses_item(item, output_index, &mut events);
                    }
                }
            }
            Some("error") | Some("response.failed") => {
                return Err(responses_stream_error(value));
            }
            Some(kind @ ("response.completed" | "response.incomplete" | "response.done")) => {
                let response = value.get("response").unwrap_or(&Value::Null);
                if kind == "response.done"
                    && response.get("status").and_then(Value::as_str) == Some("failed")
                {
                    return Err(responses_stream_error(value));
                }
                merge_usage(&mut self.usage, responses_usage(response.get("usage")));
                // Some gateways only report parallel calls (or the answer text) in the final
                // response payload; recover anything the per-item events did not deliver.
                for (index, item) in response
                    .get("output")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    match item.get("type").and_then(Value::as_str) {
                        Some("function_call") => {
                            self.complete_responses_item(item, Some(index as u64), &mut events)
                        }
                        Some("message") if !self.text_seen => {
                            for part in item
                                .get("content")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                            {
                                if let Some(text) = part
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .filter(|text| !text.is_empty())
                                {
                                    events.push(ProviderStreamEvent::TextDelta {
                                        delta: text.into(),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
                let incomplete = kind == "response.incomplete"
                    || response.get("status").and_then(Value::as_str) == Some("incomplete");
                let stop = if incomplete {
                    match response
                        .pointer("/incomplete_details/reason")
                        .or_else(|| value.pointer("/incomplete_details/reason"))
                        .and_then(Value::as_str)
                    {
                        Some("max_output_tokens") => StopReason::MaxTokens,
                        Some("content_filter") => StopReason::ContentFilter,
                        _ => StopReason::Error,
                    }
                } else if self.tools.values().any(|tool| tool.streamed) {
                    StopReason::ToolUse
                } else if self.tools.is_empty() {
                    StopReason::Final
                } else {
                    // A call was announced but its arguments never arrived anywhere.
                    StopReason::Error
                };
                self.stop_reason = Some(stop);
                events.push(self.done_event());
            }
            _ => {}
        }
        Ok(events)
    }

    /// Resolves (or creates) the canonical identity for a Responses function call from whichever
    /// identifiers the current event carries, and records all of them as aliases.
    fn responses_identity(
        &mut self,
        item_id: Option<&str>,
        call_id: Option<&str>,
        output_index: Option<u64>,
        name: Option<&str>,
    ) -> String {
        let index_alias = output_index.map(|index| format!("#{index}"));
        let candidates = [item_id, call_id, index_alias.as_deref()];
        if candidates.iter().all(Option::is_none) {
            if let Some(key) = self.last_call.clone() {
                return key;
            }
        }
        let key = candidates
            .iter()
            .flatten()
            .find_map(|alias| self.aliases.get(*alias).cloned())
            .unwrap_or_else(|| {
                let key = candidates
                    .iter()
                    .flatten()
                    .next()
                    .map(|alias| (*alias).to_owned())
                    .unwrap_or_else(|| format!("responses-call-{}", self.tools.len()));
                self.tools.insert(
                    key.clone(),
                    ToolIdentity {
                        call_id: call_id.or(item_id).unwrap_or(&key).into(),
                        name: None,
                        streamed: false,
                    },
                );
                key
            });
        for alias in candidates.iter().flatten() {
            self.aliases.insert((*alias).to_owned(), key.clone());
        }
        let identity = self.tools.get_mut(&key).expect("canonical identity exists");
        if let Some(call_id) = call_id {
            // `call_id` is what tool results are matched against; prefer it over item ids.
            if !identity.streamed {
                identity.call_id = call_id.into();
            }
        }
        if identity.name.is_none() {
            identity.name = name.map(Into::into);
        }
        self.last_call = Some(key.clone());
        key
    }

    fn complete_responses_item(
        &mut self,
        item: &Value,
        output_index: Option<u64>,
        events: &mut Vec<ProviderStreamEvent>,
    ) {
        let key = self.responses_identity(
            item.get("id").and_then(Value::as_str),
            item.get("call_id").and_then(Value::as_str),
            output_index,
            item.get("name").and_then(Value::as_str),
        );
        self.complete_responses_call(&key, item.get("arguments").and_then(Value::as_str), events);
    }

    /// Forwards the complete arguments of a Responses function call whose arguments were never
    /// streamed as deltas. Calls that already streamed are left untouched, so the `.done` events
    /// never duplicate argument text.
    fn complete_responses_call(
        &mut self,
        key: &str,
        arguments: Option<&str>,
        events: &mut Vec<ProviderStreamEvent>,
    ) {
        let Some(arguments) = arguments else {
            return;
        };
        let identity = self.tools.get_mut(key).expect("identity was just resolved");
        if identity.streamed {
            return;
        }
        identity.streamed = true;
        events.push(ProviderStreamEvent::ToolArgumentsDelta {
            call_id: identity.call_id.clone(),
            name: identity.name.clone(),
            delta: arguments.into(),
        });
    }

    fn claude_messages(
        &mut self,
        value: &Value,
    ) -> Result<Vec<ProviderStreamEvent>, ProviderError> {
        let mut events = Vec::new();
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => merge_usage(
                &mut self.usage,
                claude_messages_usage(value.pointer("/message/usage")),
            ),
            Some("content_block_start") => {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .to_string();
                if let Some(block) = value.get("content_block") {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        self.tools.insert(
                            index.clone(),
                            ToolIdentity {
                                call_id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("claude-messages-call")
                                    .into(),
                                name: block.get("name").and_then(Value::as_str).map(Into::into),
                                streamed: true,
                            },
                        );
                    }
                }
                events.push(ProviderStreamEvent::BlockStarted {
                    index: index.parse().unwrap_or(0),
                });
            }
            Some("content_block_stop") => events.push(ProviderStreamEvent::BlockFinished {
                index: value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            }),
            Some("content_block_delta") => {
                match value.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => events.push(ProviderStreamEvent::TextDelta {
                        delta: value
                            .pointer("/delta/text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .into(),
                    }),
                    Some("input_json_delta") => {
                        let key = value
                            .get("index")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                            .to_string();
                        let identity = self.tools.get(&key).ok_or_else(|| {
                            ProviderError::invalid_response(
                                "Claude Messages 工具参数块缺少 tool_use 身份",
                            )
                        })?;
                        events.push(ProviderStreamEvent::ToolArgumentsDelta {
                            call_id: identity.call_id.clone(),
                            name: identity.name.clone(),
                            delta: value
                                .pointer("/delta/partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .into(),
                        });
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                merge_usage(&mut self.usage, claude_messages_usage(value.get("usage")));
                self.stop_reason = Some(
                    match value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                        Some("tool_use") => StopReason::ToolUse,
                        Some("max_tokens") => StopReason::MaxTokens,
                        Some("stop_sequence") => StopReason::StopSequence,
                        _ => StopReason::Final,
                    },
                );
                events.push(self.done_event());
            }
            _ => {}
        }
        Ok(events)
    }

    fn done_event(&mut self) -> ProviderStreamEvent {
        self.done = true;
        ProviderStreamEvent::Done {
            stop_reason: self.stop_reason.unwrap_or(StopReason::Error),
            usage: self.usage.clone(),
        }
    }

    fn finish(&mut self) -> Option<ProviderStreamEvent> {
        if self.done {
            return None;
        }
        Some(self.done_event())
    }
}

fn merge_usage(target: &mut Usage, incoming: Usage) {
    if incoming.input_tokens > 0 {
        target.input_tokens = incoming.input_tokens;
    }
    if incoming.output_tokens > 0 {
        target.output_tokens = incoming.output_tokens;
    }
    if incoming.total_tokens > 0 {
        target.total_tokens = incoming.total_tokens;
    }
    if incoming.cache_read_input_tokens.is_some() {
        target.cache_read_input_tokens = incoming.cache_read_input_tokens;
    }
    if incoming.cache_write_input_tokens.is_some() {
        target.cache_write_input_tokens = incoming.cache_write_input_tokens;
    }
    if incoming.reasoning_tokens.is_some() {
        target.reasoning_tokens = incoming.reasoning_tokens;
    }
    if target.total_tokens == 0 {
        target.total_tokens = target.input_tokens.saturating_add(target.output_tokens);
    }
}

fn chat_image(source: &ImageSource) -> Value {
    json!({"type":"image_url", "image_url":{"url":image_url(source)}})
}

fn responses_image(source: &ImageSource) -> Value {
    json!({"type":"input_image", "image_url":image_url(source)})
}

fn claude_messages_image(source: &ImageSource) -> Value {
    match source {
        ImageSource::Url { url } => json!({"type":"image", "source":{"type":"url", "url":url}}),
        ImageSource::Base64 { media_type, data } => json!({
            "type":"image", "source":{"type":"base64", "media_type":media_type, "data":data}
        }),
    }
}

fn image_url(source: &ImageSource) -> String {
    match source {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
    }
}

fn image_content(part: &ContentPart) -> Option<ImageSource> {
    match part {
        ContentPart::Image { source } => Some(source.clone()),
        _ => None,
    }
}

fn tool_result_text(parts: &[ContentPart], is_error: bool) -> String {
    let text = parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    if is_error {
        format!("ERROR: {text}")
    } else {
        text
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn openai_tool_choice(choice: &ToolChoice, responses: bool) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Named(name) if responses => json!({"type":"function", "name":name}),
        ToolChoice::Named(name) => json!({"type":"function", "function":{"name":name}}),
    }
}

fn claude_messages_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type":"auto"}),
        ToolChoice::None => json!({"type":"none"}),
        ToolChoice::Required => json!({"type":"any"}),
        ToolChoice::Named(name) => json!({"type":"tool", "name":name}),
    }
}

fn openai_response_format(format: &ResponseFormat, responses: bool) -> Value {
    match format {
        ResponseFormat::JsonObject => json!({"type":"json_object"}),
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } if responses => json!({
            "type":"json_schema", "name":name, "schema":schema, "strict":strict
        }),
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => json!({
            "type":"json_schema", "json_schema":{"name":name, "schema":schema, "strict":strict}
        }),
    }
}

fn structured_output(
    value: &Value,
    request: &ProviderRequest,
) -> Result<Option<Value>, ProviderError> {
    if request.response_format.is_none() {
        return Ok(None);
    }
    let text = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find_map(|item| {
                    item.get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .find_map(|content| content.get("text").and_then(Value::as_str))
                })
        })
        .ok_or_else(|| ProviderError::invalid_response("响应缺少结构化 JSON 内容"))?;
    serde_json::from_str(text).map(Some).map_err(|error| {
        ProviderError::invalid_response(format!("结构化输出不是有效 JSON: {error}"))
    })
}

fn openai_usage(value: Option<&Value>) -> Usage {
    let input = value
        .and_then(|v| v.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .and_then(|v| v.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: value
            .and_then(|v| v.get("total_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(input + output),
        cache_read_input_tokens: value
            .and_then(|v| v.pointer("/prompt_tokens_details/cached_tokens"))
            .and_then(Value::as_u64),
        cache_write_input_tokens: None,
        reasoning_tokens: value
            .and_then(|v| v.pointer("/completion_tokens_details/reasoning_tokens"))
            .and_then(Value::as_u64),
    }
}

fn responses_usage(value: Option<&Value>) -> Usage {
    let input = value
        .and_then(|v| v.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .and_then(|v| v.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: value
            .and_then(|v| v.get("total_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(input + output),
        cache_read_input_tokens: value
            .and_then(|v| v.pointer("/input_tokens_details/cached_tokens"))
            .and_then(Value::as_u64),
        cache_write_input_tokens: None,
        reasoning_tokens: value
            .and_then(|v| v.pointer("/output_tokens_details/reasoning_tokens"))
            .and_then(Value::as_u64),
    }
}

fn claude_messages_usage(value: Option<&Value>) -> Usage {
    let input = value
        .and_then(|v| v.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .and_then(|v| v.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        cache_read_input_tokens: value
            .and_then(|v| v.get("cache_read_input_tokens"))
            .and_then(Value::as_u64),
        cache_write_input_tokens: value
            .and_then(|v| v.get("cache_creation_input_tokens"))
            .and_then(Value::as_u64),
        reasoning_tokens: None,
    }
}

fn parse_arguments(value: Option<&Value>) -> Result<Value, ProviderError> {
    match value {
        Some(Value::String(value)) => serde_json::from_str(value)
            .map_err(|error| ProviderError::invalid(format!("工具参数不是有效 JSON: {error}"))),
        Some(value) => Ok(value.clone()),
        None => Ok(json!({})),
    }
}

fn string_at(value: &Value, pointer: &str) -> Result<String, ProviderError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(Into::into)
        .ok_or_else(|| ProviderError::invalid(format!("响应缺少字符串字段 {pointer}")))
}

fn network_error(error: impl Display) -> ProviderError {
    ProviderError {
        kind: ErrorKind::Network,
        retriable: true,
        http_status: None,
        provider_code: None,
        message: error.to_string(),
    }
}

fn request_error(error: reqwest::Error) -> ProviderError {
    ProviderError {
        kind: if error.is_timeout() {
            ErrorKind::Timeout
        } else {
            ErrorKind::Network
        },
        retriable: error.is_timeout() || error.is_connect() || error.is_request(),
        http_status: error.status().map(|status| status.as_u16()),
        provider_code: None,
        message: error.to_string(),
    }
}

use std::fmt::Display;

const MAX_ERROR_BODY_CHARS: usize = 512;

async fn http_error(response: Response, api_key: &str) -> ProviderError {
    let status = response.status();
    let path = response.url().path().to_owned();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_owned();
    let body = response.text().await.unwrap_or_default();
    let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .map(|message| redact_error_text(message, api_key))
        .unwrap_or_else(|| {
            let preview = redact_error_text(&body, api_key);
            if preview.is_empty() {
                format!("HTTP {} from {path} ({content_type})", status.as_u16())
            } else {
                format!(
                    "HTTP {} from {path} returned non-JSON error ({content_type}): {preview}",
                    status.as_u16()
                )
            }
        });
    let provider_code = value
        .pointer("/error/code")
        .or_else(|| value.pointer("/error/type"))
        .and_then(Value::as_str)
        .map(Into::into);
    let (kind, retriable) = classify_status(status, &message);
    ProviderError {
        kind,
        retriable,
        http_status: Some(status.as_u16()),
        provider_code,
        message,
    }
}

fn redact_error_text(value: &str, api_key: &str) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = if api_key.is_empty() {
        normalized
    } else {
        normalized.replace(api_key, "***")
    };
    let mut output = redacted
        .chars()
        .take(MAX_ERROR_BODY_CHARS)
        .collect::<String>();
    if redacted.chars().count() > MAX_ERROR_BODY_CHARS {
        output.push('…');
    }
    output
}

fn classify_status(status: StatusCode, message: &str) -> (ErrorKind, bool) {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => (ErrorKind::Auth, false),
        StatusCode::TOO_MANY_REQUESTS => (ErrorKind::RateLimit, true),
        StatusCode::REQUEST_TIMEOUT => (ErrorKind::Timeout, true),
        status if status.is_server_error() => (ErrorKind::Server, true),
        StatusCode::BAD_REQUEST if message.to_ascii_lowercase().contains("context") => {
            (ErrorKind::ContextLength, false)
        }
        StatusCode::BAD_REQUEST => (ErrorKind::InvalidRequest, false),
        _ => (ErrorKind::Unknown, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderConfig;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    fn read_http_request(socket: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 8192];
        let header_end = loop {
            let size = socket.read(&mut buffer).unwrap();
            assert!(size > 0, "HTTP request ended before headers");
            bytes.extend_from_slice(&buffer[..size]);
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.strip_prefix("Content-Length:")
                    .or_else(|| line.strip_prefix("content-length:"))
            })
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let size = socket.read(&mut buffer).unwrap();
            assert!(size > 0, "HTTP request ended before body");
            bytes.extend_from_slice(&buffer[..size]);
        }
        String::from_utf8_lossy(&bytes[..header_end + content_length]).into_owned()
    }

    fn assert_rivo_user_agent(request: &str) {
        let values = request
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
            .map(|(_, value)| value.trim())
            .collect::<Vec<_>>();
        assert_eq!(values, vec![RIVO_USER_AGENT]);
        assert!(!request.to_ascii_lowercase().contains("reqwest"));
    }

    fn write_json_response(socket: &mut TcpStream, body: &str) {
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
    }

    #[test]
    fn model_discovery_normalizes_url_and_model_ids() {
        assert_eq!(
            models_url(
                ProviderKind::OpenAiChatCompletions,
                "https://example.test/v1/"
            ),
            "https://example.test/v1/models"
        );
        assert_eq!(
            models_url(ProviderKind::OpenAiChatCompletions, "https://example.test"),
            "https://example.test/v1/models"
        );
        let models = parse_model_list(
            ProviderKind::OpenAiChatCompletions,
            json!({"data":[{"id":" z "},{"id":"a"},{"id":"a"},{"id":""},{"name":"ignored"}]}),
        )
        .unwrap();
        assert_eq!(models, vec!["z", "a"]);
        assert!(parse_model_list(ProviderKind::OpenAiChatCompletions, json!({"data":{}})).is_err());
    }

    #[test]
    fn provider_request_urls_preserve_protocol_prefixes_without_duplicate_v1() {
        assert_eq!(
            provider_request_url(
                ProviderKind::OpenAiChatCompletions,
                "https://gateway.example/v1/",
                "model"
            ),
            "https://gateway.example/v1/chat/completions"
        );
        assert_eq!(
            provider_request_url(
                ProviderKind::OpenAiResponses,
                "https://gateway.example/v1",
                "model"
            ),
            "https://gateway.example/v1/responses"
        );
        assert_eq!(
            provider_request_url(
                ProviderKind::ClaudeMessages,
                "https://gateway.example",
                "model"
            ),
            "https://gateway.example/v1/messages"
        );
        assert_eq!(
            provider_request_url(
                ProviderKind::ClaudeMessages,
                "https://gateway.example/v1/",
                "model"
            ),
            "https://gateway.example/v1/messages"
        );
    }

    #[test]
    fn computer_use_flag_adds_native_provider_tools() {
        let mut request = ProviderRequest::text("computer-model", "inspect the desktop");
        request.enable_computer_use();
        request.tools.push(rivo_core::ToolDefinition {
            name: "computer".into(),
            description: "internal function tool".into(),
            parameters: json!({"type":"object"}),
        });
        let responses = build_responses_request(&request, false).unwrap();
        assert_eq!(
            responses["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["type"] == "computer")
                .count(),
            1
        );
        let claude = build_claude_messages_request(&request, false).unwrap();
        assert_eq!(
            claude["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["name"] == "computer")
                .count(),
            1
        );
        let chat = build_chat_request(&request, false).unwrap();
        assert_eq!(
            chat["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["function"]["name"] == "computer")
                .count(),
            1
        );
    }

    #[test]
    fn computer_use_function_fallback_uses_responses_function_tool_and_output() {
        let mut request = ProviderRequest::text("computer-model", "inspect the desktop");
        request.enable_computer_use();
        request.tools.push(rivo_core::ToolDefinition {
            name: "computer".into(),
            description: "internal function tool".into(),
            parameters: json!({"type":"object"}),
        });
        request
            .metadata
            .insert("rivo.computer_use_function_fallback".into(), "true".into());
        let responses = build_responses_request(&request, false).unwrap();
        let tools = responses["tools"].as_array().unwrap();
        assert_eq!(
            tools
                .iter()
                .filter(|tool| tool["name"] == "computer")
                .count(),
            1
        );
        let computer = tools
            .iter()
            .find(|tool| tool["name"] == "computer")
            .unwrap();
        assert_eq!(computer["type"], "function");
        assert_eq!(computer["parameters"]["additionalProperties"], false);
        for field in [
            "window_id",
            "max_nodes",
            "from_x",
            "from_y",
            "to_x",
            "to_y",
            "delta",
            "button",
            "timeout_ms",
        ] {
            assert!(
                computer["parameters"]["properties"].get(field).is_some(),
                "missing computer function field: {field}"
            );
        }
        assert_eq!(
            computer["parameters"]["properties"]["action"]["enum"],
            json!([
                "screenshot",
                "list_windows",
                "ui_snapshot",
                "focus_window",
                "mouse_move",
                "click",
                "double_click",
                "drag",
                "scroll",
                "type",
                "key",
                "wait"
            ])
        );
    }

    #[test]
    fn computer_use_native_responses_tool_remains_native() {
        let mut request = ProviderRequest::text("computer-model", "inspect the desktop");
        request.enable_computer_use();
        let responses = build_responses_request(&request, false).unwrap();
        assert_eq!(responses["tools"].as_array().unwrap().len(), 1);
        assert_eq!(responses["tools"][0]["type"], "computer");
        assert!(responses["tools"][0].get("parameters").is_none());
    }

    #[test]
    fn reasoning_effort_maps_per_protocol_and_omits_provider_default() {
        let mut request = ProviderRequest::text("model", "hello");
        assert!(build_chat_request(&request, false)
            .unwrap()
            .get("reasoning_effort")
            .is_none());
        request.reasoning_effort = ReasoningEffort::Medium;
        let chat = build_chat_request(&request, false).unwrap();
        assert_eq!(chat["reasoning_effort"], json!("medium"));
        let responses = build_responses_request(&request, false).unwrap();
        assert_eq!(responses["reasoning"]["effort"], json!("medium"));
        let claude = build_claude_messages_request(&request, false).unwrap();
        assert_eq!(claude["thinking"]["type"], json!("enabled"));
        assert_eq!(claude["thinking"]["budget_tokens"], json!(4096));
    }

    #[test]
    fn claude_system_prompt_uses_single_ephemeral_cache_breakpoint() {
        let request = ProviderRequest {
            system: Some("stable instructions".into()),
            ..ProviderRequest::text("claude", "dynamic user prompt")
        };
        let body = build_claude_messages_request(&request, false).unwrap();
        assert_eq!(
            body.pointer("/system/0/type").and_then(Value::as_str),
            Some("text")
        );
        assert_eq!(
            body.pointer("/system/0/text").and_then(Value::as_str),
            Some("stable instructions")
        );
        assert_eq!(
            body.pointer("/system/0/cache_control/type")
                .and_then(Value::as_str),
            Some("ephemeral")
        );
        assert_eq!(
            body.pointer("/system")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    fn normalize(kind: ProviderKind, fixture: Value) -> ProviderResponse {
        normalize_response(
            kind,
            fixture,
            None,
            &ProviderRequest::text("fallback", "test"),
        )
        .unwrap()
    }

    #[test]
    fn all_protocols_normalize_text_usage_and_stop_reason() {
        let chat = normalize(
            ProviderKind::OpenAiChatCompletions,
            json!({
                "model":"chat-model", "choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}
            }),
        );
        let responses = normalize(
            ProviderKind::OpenAiResponses,
            json!({
                "id":"r1", "model":"responses-model", "status":"completed",
                "output":[{"type":"message","content":[{"type":"output_text","text":"hello"}]}],
                "usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}
            }),
        );
        let claude_messages = normalize(
            ProviderKind::ClaudeMessages,
            json!({
                "id":"a1", "model":"anthropic-model", "stop_reason":"end_turn",
                "content":[{"type":"text","text":"hello"}],
                "usage":{"input_tokens":2,"output_tokens":3}
            }),
        );
        for response in [chat, responses, claude_messages] {
            assert_eq!(response.message.text_content(), "hello");
            assert_eq!(response.stop_reason, StopReason::Final);
            assert_eq!(response.usage.total_tokens, 5);
        }
    }

    #[test]
    fn all_protocols_normalize_tool_calls() {
        let chat = normalize(
            ProviderKind::OpenAiChatCompletions,
            json!({
                "choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","function":{"name":"read_file","arguments":"{\"path\":\"README.md\"}"}}]},"finish_reason":"tool_calls"}]
            }),
        );
        let responses = normalize(
            ProviderKind::OpenAiResponses,
            json!({
                "status":"completed", "output":[{"type":"function_call","call_id":"c1","name":"read_file","arguments":"{\"path\":\"README.md\"}"}]
            }),
        );
        let claude_messages = normalize(
            ProviderKind::ClaudeMessages,
            json!({
                "stop_reason":"tool_use", "content":[{"type":"tool_use","id":"c1","name":"read_file","input":{"path":"README.md"}}]
            }),
        );
        for response in [chat, responses, claude_messages] {
            assert_eq!(response.stop_reason, StopReason::ToolUse);
            assert!(matches!(
                response.message.content[0],
                ContentPart::ToolUse { .. }
            ));
        }
    }

    #[test]
    fn stream_deltas_are_normalized_for_all_protocols() {
        let mut chat_accumulator = StreamAccumulator::new(ProviderKind::OpenAiChatCompletions);
        let chat = chat_accumulator
            .normalize(&json!({
                "choices":[{"delta":{"content":"a"},"finish_reason":null}]
            }))
            .unwrap();
        let mut responses_accumulator = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let responses = responses_accumulator
            .normalize(&json!({
                "type":"response.output_text.delta", "delta":"a"
            }))
            .unwrap();
        let mut claude_messages_accumulator = StreamAccumulator::new(ProviderKind::ClaudeMessages);
        let claude_messages = claude_messages_accumulator
            .normalize(&json!({
                "type":"content_block_delta", "delta":{"type":"text_delta","text":"a"}
            }))
            .unwrap();
        for events in [chat, responses, claude_messages] {
            assert_eq!(
                events,
                vec![ProviderStreamEvent::TextDelta { delta: "a".into() }]
            );
        }
    }

    #[test]
    fn claude_messages_json_schema_uses_forced_result_tool() {
        let request = ProviderRequest {
            response_format: Some(ResponseFormat::JsonSchema {
                name: "result".into(),
                schema: json!({"type":"object"}),
                strict: true,
            }),
            ..ProviderRequest::text("claude", "hello")
        };
        let body = build_claude_messages_request(&request, false).unwrap();
        assert_eq!(body.pointer("/tool_choice/name"), Some(&json!("result")));

        let response = normalize_response(
            ProviderKind::ClaudeMessages,
            json!({
                "model":"claude", "stop_reason":"tool_use",
                "content":[{"type":"tool_use","id":"result-1","name":"result","input":{"ok":true}}]
            }),
            None,
            &request,
        )
        .unwrap();
        assert_eq!(response.stop_reason, StopReason::Final);
        assert_eq!(response.structured_output, Some(json!({"ok":true})));
    }

    #[test]
    fn claude_messages_structured_output_missing_result_is_an_error() {
        let request = ProviderRequest {
            response_format: Some(ResponseFormat::JsonObject),
            ..ProviderRequest::text("claude", "hello")
        };
        let error = normalize_response(
            ProviderKind::ClaudeMessages,
            json!({
                "model":"claude", "stop_reason":"end_turn",
                "content":[{"type":"text","text":"not a structured result"}]
            }),
            None,
            &request,
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidResponse);
    }

    #[test]
    fn claude_messages_structured_output_accepts_json_text_from_compatibility_gateways() {
        let request = ProviderRequest {
            response_format: Some(ResponseFormat::JsonObject),
            ..ProviderRequest::text("test-model", "return json")
        };
        let response = normalize_response(
            ProviderKind::ClaudeMessages,
            json!({
                "model":"test-model", "stop_reason":"end_turn",
                "content":[{"type":"text","text":"{\"status\":\"ok\"}"}]
            }),
            None,
            &request,
        )
        .unwrap();
        assert_eq!(response.structured_output, Some(json!({"status":"ok"})));
        assert_eq!(response.stop_reason, StopReason::Final);
    }

    #[test]
    fn images_and_json_schema_map_to_each_protocol() {
        let mut request = ProviderRequest::text("model", "hello");
        request.messages[0].content.push(ContentPart::Image {
            source: ImageSource::Url {
                url: "https://example.com/image.png".into(),
            },
        });
        request.response_format = Some(ResponseFormat::JsonSchema {
            name: "result".into(),
            schema: json!({"type":"object"}),
            strict: true,
        });
        let chat = build_chat_request(&request, false).unwrap();
        let responses = build_responses_request(&request, false).unwrap();
        let claude_messages = build_claude_messages_request(&request, false).unwrap();
        assert_eq!(
            chat.pointer("/messages/0/content/1/type"),
            Some(&json!("image_url"))
        );
        assert_eq!(
            chat.pointer("/response_format/json_schema/name"),
            Some(&json!("result"))
        );
        assert_eq!(
            responses.pointer("/input/0/content/1/type"),
            Some(&json!("input_image"))
        );
        assert_eq!(
            responses.pointer("/text/format/name"),
            Some(&json!("result"))
        );
        assert_eq!(
            claude_messages.pointer("/messages/0/content/1/type"),
            Some(&json!("image"))
        );
    }

    #[tokio::test]
    async fn openai_images_fixture_covers_generation_and_edit_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut generation, _) = listener.accept().unwrap();
            let request = read_http_request(&mut generation);
            assert!(request.starts_with("POST /v1/images/generations HTTP/1.1"));
            assert_rivo_user_agent(&request);
            let body = request.split_once("\r\n\r\n").unwrap().1;
            let body: Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["model"], "gpt-image-1");
            assert_eq!(body["prompt"], "an orange cat");
            write_json_response(&mut generation, r#"{"data":[{"b64_json":"aGVsbG8="}]}"#);

            let (mut edit, _) = listener.accept().unwrap();
            let request = read_http_request(&mut edit);
            assert!(request.starts_with("POST /v1/images/edits HTTP/1.1"));
            assert_rivo_user_agent(&request);
            assert!(request.contains("name=\"model\""));
            assert!(request.contains("gpt-image-1"));
            assert!(request.contains("name=\"prompt\""));
            assert!(request.contains("turn it into an oil painting"));
            assert!(request.contains("name=\"image[]\""));
            assert!(request.contains("input.png"));
            write_json_response(&mut edit, r#"{"data":[{"b64_json":"d29ybGQ="}]}"#);
        });
        let client = create_media_client(
            ProviderKind::OpenAiChatCompletions,
            ProviderConfig {
                base_url: format!("http://{address}/v1"),
                api_key: "fixture-key".into(),
                timeout_seconds: 5,
                max_stream_event_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let generated = client
            .execute_media(crate::MediaProviderRequest {
                model: "gpt-image-1".into(),
                operation: MediaOperation::Generate,
                prompt: "an orange cat".into(),
                inputs: vec![],
                size: Some("1024x1024".into()),
                output_media_type: Some("image/png".into()),
            })
            .await
            .unwrap();
        assert_eq!(generated.data, b"hello");
        assert_eq!(generated.media_type, "image/png");
        let edited = client
            .execute_media(crate::MediaProviderRequest {
                model: "gpt-image-1".into(),
                operation: MediaOperation::Edit,
                prompt: "turn it into an oil painting".into(),
                inputs: vec![crate::MediaInput {
                    media_type: "image/png".into(),
                    data: b"source".to_vec(),
                }],
                size: None,
                output_media_type: Some("image/png".into()),
            })
            .await
            .unwrap();
        assert_eq!(edited.data, b"world");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn openai_responses_image_generation_fixture_covers_edit_input() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let request = read_http_request(&mut socket);
            assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
            let body = request.split_once("\r\n\r\n").unwrap().1;
            let body: Value = serde_json::from_str(body).unwrap();
            assert_eq!(body["model"], "gpt-6-astra");
            assert_eq!(body["tools"][0]["type"], "image_generation");
            assert_eq!(body["input"][0]["content"][0]["text"], "oil painting");
            assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
            assert!(body["input"][0]["content"][1]["image_url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"));
            write_json_response(
                &mut socket,
                r#"{"output":[{"type":"image_generation_call","result":"aGVsbG8="}]}"#,
            );
        });
        let client = create_media_client(
            ProviderKind::OpenAiResponses,
            ProviderConfig {
                base_url: format!("http://{address}/v1"),
                api_key: "fixture-key".into(),
                timeout_seconds: 5,
                max_stream_event_bytes: 1024 * 1024,
            },
        )
        .unwrap();
        let result = client
            .execute_media(crate::MediaProviderRequest {
                model: "gpt-6-astra".into(),
                operation: MediaOperation::Edit,
                prompt: "oil painting".into(),
                inputs: vec![crate::MediaInput {
                    media_type: "image/png".into(),
                    data: b"source".to_vec(),
                }],
                size: Some("1024x1024".into()),
                output_media_type: Some("image/png".into()),
            })
            .await
            .unwrap();
        assert_eq!(result.data, b"hello");
        server.join().unwrap();
    }

    #[test]
    fn provider_errors_have_stable_retry_classification() {
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED, "bad key"),
            (ErrorKind::Auth, false)
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, "slow down"),
            (ErrorKind::RateLimit, true)
        );
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR, "upstream"),
            (ErrorKind::Server, true)
        );
        assert_eq!(
            classify_status(StatusCode::BAD_REQUEST, "context length exceeded"),
            (ErrorKind::ContextLength, false)
        );
    }

    #[tokio::test]
    async fn transient_http_errors_are_retried_with_a_limit() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();
        let server = thread::spawn(move || {
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 8192];
                let size = socket.read(&mut buffer).unwrap();
                assert_rivo_user_agent(&String::from_utf8_lossy(&buffer[..size]));
                let attempt = server_attempts.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if attempt < 2 {
                    ("429 Too Many Requests", r#"{"error":{"message":"retry"}}"#)
                } else {
                    (
                        "200 OK",
                        r#"{"model":"mock","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                    )
                };
                write!(
                    socket,
                    "HTTP/1.1 {status}
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let client = OpenAiChatCompletionsClient::new(ProviderConfig {
            base_url: format!("http://{address}"),
            api_key: "test".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024 * 1024,
        })
        .unwrap();
        let response = client
            .complete(ProviderRequest::text("mock", "hello"))
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(response.message.text_content(), "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn claude_messages_versioned_base_posts_to_single_v1_messages_path() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let size = socket.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
            assert_rivo_user_agent(&request);
            let body = r#"{"model":"mock","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"output_tokens":1}}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK
Content-Type: application/json
Content-Length: {}
Connection: close

{body}",
                body.len()
            )
            .unwrap();
        });
        let client = ClaudeMessagesClient::new(ProviderConfig {
            base_url: format!("http://{address}/v1/"),
            api_key: "test".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024 * 1024,
        })
        .unwrap();

        let response = client
            .complete(ProviderRequest::text("mock", "hello"))
            .await
            .unwrap();
        server.join().unwrap();
        assert_eq!(response.message.text_content(), "ok");
    }

    #[tokio::test]
    async fn claude_messages_sends_ephemeral_cache_breakpoint_on_wire() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 16 * 1024];
            let size = socket.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..size]);
            let body = request
                .split_once("\r\n\r\n")
                .map(|(_, body)| body)
                .expect("HTTP request body");
            let body: Value = serde_json::from_str(body).expect("JSON request body");
            assert_eq!(
                body.pointer("/system/0/cache_control/type")
                    .and_then(Value::as_str),
                Some("ephemeral")
            );
            assert_eq!(
                body.pointer("/system")
                    .and_then(Value::as_array)
                    .map(Vec::len),
                Some(1)
            );
            let response_body = r#"{"model":"mock","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"output_tokens":1}}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();
        });
        let client = ClaudeMessagesClient::new(ProviderConfig {
            base_url: format!("http://{address}"),
            api_key: "test".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024 * 1024,
        })
        .unwrap();
        let request = ProviderRequest {
            system: Some("stable instructions".into()),
            ..ProviderRequest::text("mock", "dynamic prompt")
        };
        let response = client.complete(request).await.unwrap();
        server.join().unwrap();
        assert_eq!(response.message.text_content(), "ok");
    }

    #[tokio::test]
    async fn non_json_http_errors_preserve_route_status_and_redact_api_key() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let _ = socket.read(&mut buffer).unwrap();
            let body = "<html><body>route not found; token=super-secret-key</body></html>";
            write!(
                socket,
                "HTTP/1.1 404 Not Found
Content-Type: text/html
Content-Length: {}
Connection: close

{body}",
                body.len()
            )
            .unwrap();
        });
        let client = OpenAiResponsesClient::new(ProviderConfig {
            base_url: format!("http://{address}/v1"),
            api_key: "super-secret-key".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024 * 1024,
        })
        .unwrap();

        let error = client
            .complete(ProviderRequest::text("mock", "hello"))
            .await
            .unwrap_err();
        server.join().unwrap();

        assert_eq!(error.kind, ErrorKind::Unknown);
        assert_eq!(error.http_status, Some(404));
        assert!(error.message.contains("HTTP 404 from /v1/responses"));
        assert!(error.message.contains("route not found"));
        assert!(!error.message.contains("super-secret-key"));
        assert!(error.to_string().contains("HTTP 404"));
    }

    #[tokio::test]
    async fn claude_http_errors_redact_x_api_key_and_preserve_route_status() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 8192];
            let size = socket.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..size]).to_ascii_lowercase();
            assert!(request.starts_with("post /v1/messages http/1.1"));
            assert!(request.contains("x-api-key: claude-secret-key"));
            let body = "{\"type\":\"error\",\"message\":\"invalid key claude-secret-key\"}";
            write!(
                socket,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let client = ClaudeMessagesClient::new(ProviderConfig {
            base_url: format!("http://{address}/v1"),
            api_key: "claude-secret-key".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024 * 1024,
        })
        .unwrap();

        let error = client
            .complete(ProviderRequest::text("mock", "hello"))
            .await
            .unwrap_err();
        server.join().unwrap();

        assert_eq!(error.kind, ErrorKind::Auth);
        assert_eq!(error.http_status, Some(401));
        assert!(error.message.contains("invalid key"));
        assert!(!error.message.contains("claude-secret-key"));
        assert!(!error.to_string().contains("claude-secret-key"));
    }

    fn loopback_response(body: &str, content_type: &str, expected_path: &str) -> ProviderConfig {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_owned();
        let content_type = content_type.to_owned();
        let expected_path = expected_path.to_owned();
        thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 16 * 1024];
            let size = socket.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request.contains(&format!("POST {expected_path} HTTP/1.1")));
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        ProviderConfig {
            base_url: format!("http://{address}"),
            api_key: "loopback-key".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024 * 1024,
        }
    }

    #[tokio::test]
    async fn loopback_complete_and_stream_cover_all_standard_protocol_clients() {
        let chat_config = loopback_response(
            r#"{"id":"c1","model":"test-model","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            "application/json",
            "/chat/completions",
        );
        let chat = OpenAiChatCompletionsClient::new(chat_config).unwrap();
        assert_eq!(
            chat.complete(ProviderRequest::text("test-model", "hello"))
                .await
                .unwrap()
                .message
                .text_content(),
            "ok"
        );

        let responses_config = loopback_response(
            r#"{"id":"r1","model":"test-model","status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}"#,
            "application/json",
            "/responses",
        );
        let responses = OpenAiResponsesClient::new(responses_config).unwrap();
        assert_eq!(
            responses
                .complete(ProviderRequest::text("test-model", "hello"))
                .await
                .unwrap()
                .message
                .text_content(),
            "ok"
        );

        let claude_config = loopback_response(
            r#"{"id":"m1","model":"test-model","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"output_tokens":1}}"#,
            "application/json",
            "/v1/messages",
        );
        let claude = ClaudeMessagesClient::new(claude_config).unwrap();
        assert_eq!(
            claude
                .complete(ProviderRequest::text("test-model", "hello"))
                .await
                .unwrap()
                .message
                .text_content(),
            "ok"
        );
    }

    #[tokio::test]
    async fn loopback_streams_cover_sse_protocol_termination() {
        let chat = OpenAiChatCompletionsClient::new(loopback_response(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
            "/chat/completions",
        ))
        .unwrap();
        let chat_events = chat
            .stream(ProviderRequest::text("test-model", "hello"))
            .collect::<Vec<_>>()
            .await;
        assert!(chat_events.iter().any(
            |event| matches!(event, Ok(ProviderStreamEvent::TextDelta { delta }) if delta == "ok")
        ));

        let responses = OpenAiResponsesClient::new(loopback_response(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            "text/event-stream",
            "/responses",
        ))
        .unwrap();
        let responses_events = responses
            .stream(ProviderRequest::text("test-model", "hello"))
            .collect::<Vec<_>>()
            .await;
        assert!(responses_events.iter().any(|event| matches!(
            event,
            Ok(ProviderStreamEvent::TextDelta { delta }) if delta == "ok"
        )));

        let claude = ClaudeMessagesClient::new(loopback_response(
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "text/event-stream",
            "/v1/messages",
        ))
        .unwrap();
        let claude_events = claude
            .stream(ProviderRequest::text("test-model", "hello"))
            .collect::<Vec<_>>()
            .await;
        assert!(claude_events.iter().any(|event| matches!(
            event,
            Ok(ProviderStreamEvent::TextDelta { delta }) if delta == "ok"
        )));
    }

    #[tokio::test]
    async fn dropping_claude_stream_cancels_the_inflight_http_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let size = socket.read(&mut buffer).await.unwrap();
                assert!(size > 0);
                request.extend_from_slice(&buffer[..size]);
            }
            let event = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"first\"}}\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n{event}").as_bytes()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                }
            })
            .await
            .expect("dropping a Claude stream must release its HTTP response");
        });
        let client = ClaudeMessagesClient::new(ProviderConfig {
            base_url: format!("http://{address}"),
            api_key: "cancellation-fixture-key".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024,
        })
        .unwrap();
        let mut stream = client.stream(ProviderRequest::text("fixture", "cancel"));
        let event = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(event, ProviderStreamEvent::TextDelta { delta } if delta == "first"));
        drop(stream);
        server.await.unwrap();
    }

    #[test]
    fn sse_parser_preserves_utf8_across_network_chunks() {
        let mut buffer = Vec::new();
        let mut data = Vec::new();
        let bytes = "data: {\"text\":\"你好🙂\"}\n\n".as_bytes();
        let split = "data: {\"text\":\"你".len();
        assert!(parse_sse(&mut buffer, &mut data, &bytes[..split], 1024)
            .unwrap()
            .is_empty());
        let payloads = parse_sse(&mut buffer, &mut data, &bytes[split..], 1024).unwrap();
        assert_eq!(payloads, vec!["{\"text\":\"你好🙂\"}".to_string()]);
    }

    #[test]
    fn stream_accumulator_keeps_claude_messages_tool_identity_and_usage() {
        let mut accumulator = StreamAccumulator::new(ProviderKind::ClaudeMessages);
        accumulator
            .normalize(&json!({"type":"message_start","message":{"usage":{"input_tokens":4}}}))
            .unwrap();
        accumulator.normalize(&json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool-1","name":"read_file"}})).unwrap();
        let events = accumulator.normalize(&json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}})).unwrap();
        assert!(
            matches!(&events[0], ProviderStreamEvent::ToolArgumentsDelta { call_id, name: Some(name), .. } if call_id == "tool-1" && name == "read_file")
        );
    }

    #[test]
    fn chat_stream_keeps_usage_arriving_after_finish_reason() {
        let mut accumulator = StreamAccumulator::new(ProviderKind::OpenAiChatCompletions);
        let finish_events = accumulator
            .normalize(&json!({
                "choices":[{"delta":{},"finish_reason":"stop"}]
            }))
            .unwrap();
        assert!(finish_events.is_empty());
        accumulator
            .normalize(&json!({
                "choices":[],
                "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}
            }))
            .unwrap();
        let Some(ProviderStreamEvent::Done { usage, stop_reason }) = accumulator.finish() else {
            panic!("chat stream should finish at [DONE]")
        };
        assert_eq!(stop_reason, StopReason::Final);
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.total_tokens, 10);
    }

    #[test]
    fn chat_mapping_preserves_multiple_tool_results() {
        let request = ProviderRequest {
            messages: vec![Message {
                role: Role::Tool,
                content: vec![
                    ContentPart::ToolResult {
                        tool_call_id: "a".into(),
                        content: vec![ContentPart::Text { text: "one".into() }],
                        is_error: false,
                    },
                    ContentPart::ToolResult {
                        tool_call_id: "b".into(),
                        content: vec![ContentPart::Text { text: "two".into() }],
                        is_error: false,
                    },
                ],
            }],
            ..ProviderRequest::text("m", "q")
        };
        let body = build_chat_request(&request, false).unwrap();
        assert_eq!(
            body.get("messages")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn response_normalizers_reject_malformed_fields_and_preserve_incomplete_reasons() {
        let request = ProviderRequest::text("model", "task");
        let missing_choice = normalize_response(
            ProviderKind::OpenAiChatCompletions,
            json!({"choices":[]}),
            None,
            &request,
        )
        .unwrap_err();
        assert_eq!(missing_choice.kind, ErrorKind::InvalidRequest);

        let invalid_tool_arguments = normalize_response(
            ProviderKind::OpenAiResponses,
            json!({"output":[{"type":"function_call","call_id":"call","name":"tool","arguments":"{"}]}),
            None,
            &request,
        )
        .unwrap_err();
        assert_eq!(invalid_tool_arguments.kind, ErrorKind::InvalidRequest);

        for (reason, expected) in [
            ("max_output_tokens", StopReason::MaxTokens),
            ("content_filter", StopReason::ContentFilter),
            ("unknown", StopReason::Error),
        ] {
            let response = normalize_response(
                ProviderKind::OpenAiResponses,
                json!({"status":"incomplete","incomplete_details":{"reason":reason},"output":[]}),
                None,
                &request,
            )
            .unwrap();
            assert_eq!(response.stop_reason, expected);
        }
    }

    #[test]
    fn responses_stream_without_argument_deltas_still_yields_tool_calls() {
        let mut responses = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        responses
            .normalize(&json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"read_file","arguments":""}}))
            .unwrap();
        let done = responses
            .normalize(&json!({"type":"response.function_call_arguments.done","item_id":"item-1","arguments":"{\"path\":\"notes.md\"}"}))
            .unwrap();
        assert_eq!(
            done,
            vec![ProviderStreamEvent::ToolArgumentsDelta {
                call_id: "call-1".into(),
                name: Some("read_file".into()),
                delta: "{\"path\":\"notes.md\"}".into(),
            }]
        );
        let item_done = responses
            .normalize(&json!({"type":"response.output_item.done","item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"read_file","arguments":"{\"path\":\"notes.md\"}"}}))
            .unwrap();
        assert!(
            item_done.is_empty(),
            "completed arguments are forwarded once"
        );

        // Only `output_item.done` carries the arguments.
        responses
            .normalize(&json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item-2","call_id":"call-2","name":"list_dir"}}))
            .unwrap();
        let item_only = responses
            .normalize(&json!({"type":"response.output_item.done","item":{"type":"function_call","id":"item-2","call_id":"call-2","name":"list_dir","arguments":"{}"}}))
            .unwrap();
        assert!(
            matches!(&item_only[..], [ProviderStreamEvent::ToolArgumentsDelta { call_id, name: Some(name), delta }] if call_id == "call-2" && name == "list_dir" && delta == "{}")
        );
        let completed = responses
            .normalize(&json!({"type":"response.completed","response":{}}))
            .unwrap();
        assert!(matches!(
            &completed[0],
            ProviderStreamEvent::Done {
                stop_reason: StopReason::ToolUse,
                ..
            }
        ));
    }

    fn collect_tool_calls(events: &[ProviderStreamEvent]) -> Vec<(String, String)> {
        let mut calls: Vec<(String, String)> = Vec::new();
        for event in events {
            if let ProviderStreamEvent::ToolArgumentsDelta { call_id, delta, .. } = event {
                match calls.iter_mut().find(|(id, _)| id == call_id) {
                    Some((_, arguments)) => arguments.push_str(delta),
                    None => calls.push((call_id.clone(), delta.clone())),
                }
            }
        }
        calls
    }

    fn feed(accumulator: &mut StreamAccumulator, values: &[Value]) -> Vec<ProviderStreamEvent> {
        values
            .iter()
            .flat_map(|value| accumulator.normalize(value).unwrap())
            .collect()
    }

    #[test]
    fn responses_stream_keeps_interleaved_parallel_calls_apart() {
        let mut responses = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        // Deltas identify the call only by output_index on this gateway shape.
        let events = feed(
            &mut responses,
            &[
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"delegate_task"}}),
                json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_b","call_id":"call_b","name":"delegate_task"}}),
                json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"task\":"}),
                json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"task\":\"B\"}"}),
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_a","output_index":0,"delta":"\"A\"}"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_a","call_id":"call_a","name":"delegate_task","arguments":"{\"task\":\"A\"}"}}),
                json!({"type":"response.completed","response":{"status":"completed","output":[
                    {"type":"function_call","id":"fc_a","call_id":"call_a","name":"delegate_task","arguments":"{\"task\":\"A\"}"},
                    {"type":"function_call","id":"fc_b","call_id":"call_b","name":"delegate_task","arguments":"{\"task\":\"B\"}"}
                ]}}),
            ],
        );
        assert_eq!(
            collect_tool_calls(&events),
            vec![
                ("call_a".to_owned(), "{\"task\":\"A\"}".to_owned()),
                ("call_b".to_owned(), "{\"task\":\"B\"}".to_owned()),
            ]
        );
        assert!(matches!(
            events.last(),
            Some(ProviderStreamEvent::Done {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[test]
    fn responses_stream_recovers_calls_and_text_reported_only_on_completion() {
        let mut responses = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let events = feed(
            &mut responses,
            &[
                json!({"type":"response.completed","response":{"status":"completed","output":[
                    {"type":"reasoning","id":"rs_1","summary":[]},
                    {"type":"function_call","id":"fc_1","call_id":"call_1","name":"delegate_task","arguments":"{\"task\":\"A\"}"},
                    {"type":"function_call","id":"fc_2","call_id":"call_2","name":"delegate_task","arguments":"{\"task\":\"B\"}"}
                ]}}),
            ],
        );
        assert_eq!(collect_tool_calls(&events).len(), 2);
        assert!(matches!(
            events.last(),
            Some(ProviderStreamEvent::Done {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));

        let mut text_only = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let events = feed(
            &mut text_only,
            &[
                json!({"type":"response.completed","response":{"status":"completed","output":[
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}
                ]}}),
            ],
        );
        assert_eq!(
            events[0],
            ProviderStreamEvent::TextDelta {
                delta: "done".into()
            }
        );

        let mut announced_only = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let events = feed(
            &mut announced_only,
            &[
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_x","call_id":"call_x","name":"write_file"}}),
                json!({"type":"response.completed","response":{"status":"completed","output":[]}}),
            ],
        );
        assert!(
            matches!(
                events.last(),
                Some(ProviderStreamEvent::Done {
                    stop_reason: StopReason::Error,
                    ..
                })
            ),
            "a call without arguments must not look like a successful tool turn"
        );
    }

    #[test]
    fn responses_stream_surfaces_in_band_errors() {
        let mut responses = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let error = responses
            .normalize(&json!({"type":"error","code":"rate_limit_exceeded","message":"slow down"}))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::RateLimit);
        assert!(error.retriable);
        assert!(error.message.contains("slow down"));

        let mut failed = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let error = failed
            .normalize(&json!({"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"upstream exploded"}}}))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Server);
        assert_eq!(error.provider_code.as_deref(), Some("server_error"));
        assert!(error.message.contains("upstream exploded"));
    }

    #[test]
    fn responses_stream_with_argument_deltas_ignores_done_arguments() {
        let mut responses = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        responses
            .normalize(&json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"read_file"}}))
            .unwrap();
        let delta = responses
            .normalize(&json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"{\"path\":\"a\"}"}))
            .unwrap();
        assert_eq!(delta.len(), 1);
        let done = responses
            .normalize(&json!({"type":"response.function_call_arguments.done","item_id":"item-1","arguments":"{\"path\":\"a\"}"}))
            .unwrap();
        let item_done = responses
            .normalize(&json!({"type":"response.output_item.done","item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"read_file","arguments":"{\"path\":\"a\"}"}}))
            .unwrap();
        assert!(
            done.is_empty() && item_done.is_empty(),
            "streamed arguments are not duplicated"
        );
    }

    #[test]
    fn stream_accumulators_cover_responses_and_claude_messages_terminal_branches() {
        let mut responses = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let text = responses
            .normalize(&json!({"type":"response.output_text.delta","delta":"hello"}))
            .unwrap();
        assert_eq!(
            text,
            vec![ProviderStreamEvent::TextDelta {
                delta: "hello".into()
            }]
        );
        responses
            .normalize(&json!({"type":"response.output_item.added","item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"read_file"}}))
            .unwrap();
        let tool = responses
            .normalize(&json!({"type":"response.function_call_arguments.delta","item_id":"item-1","delta":"{\"path\":\"a\"}"}))
            .unwrap();
        assert!(
            matches!(&tool[0], ProviderStreamEvent::ToolArgumentsDelta { call_id, name: Some(name), .. } if call_id == "call-1" && name == "read_file")
        );
        let done = responses
            .normalize(&json!({"type":"response.completed","response":{"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}))
            .unwrap();
        assert!(
            matches!(&done[0], ProviderStreamEvent::Done { stop_reason: StopReason::ToolUse, usage } if usage.total_tokens == 5)
        );
        assert!(responses.finish().is_none());

        let mut incomplete = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let error = incomplete
            .normalize(&json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}))
            .unwrap();
        assert!(matches!(
            &error[0],
            ProviderStreamEvent::Done {
                stop_reason: StopReason::ContentFilter,
                ..
            }
        ));

        let mut incomplete = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        let events = incomplete
            .normalize(&json!({"type":"response.incomplete","incomplete_details":{"reason":"max_output_tokens"}}))
            .unwrap();
        assert!(matches!(
            &events[0],
            ProviderStreamEvent::Done {
                stop_reason: StopReason::MaxTokens,
                ..
            }
        ));

        let mut claude_messages = StreamAccumulator::new(ProviderKind::ClaudeMessages);
        let started = claude_messages
            .normalize(
                &json!({"type":"content_block_start","index":2,"content_block":{"type":"text"}}),
            )
            .unwrap();
        assert!(matches!(
            started[0],
            ProviderStreamEvent::BlockStarted { index: 2 }
        ));
        let delta = claude_messages
            .normalize(&json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"body"}}))
            .unwrap();
        assert!(matches!(&delta[0], ProviderStreamEvent::TextDelta { delta } if delta == "body"));
        let stopped = claude_messages
            .normalize(&json!({"type":"content_block_stop","index":2}))
            .unwrap();
        assert!(matches!(
            stopped[0],
            ProviderStreamEvent::BlockFinished { index: 2 }
        ));
        let done = claude_messages
            .normalize(&json!({"type":"message_delta","delta":{"stop_reason":"stop_sequence"},"usage":{"output_tokens":4}}))
            .unwrap();
        assert!(
            matches!(&done[0], ProviderStreamEvent::Done { stop_reason: StopReason::StopSequence, usage } if usage.total_tokens == 4)
        );
    }

    #[test]
    fn sse_parser_enforces_size_and_finish_semantics() {
        let mut buffer = Vec::new();
        let mut data = Vec::new();
        let oversized_line = b"x".repeat(33);
        let error = parse_sse(&mut buffer, &mut data, &oversized_line, 32).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidResponse);

        let mut buffer = b"data: final payload".to_vec();
        let mut data = Vec::new();
        assert_eq!(
            finish_sse(&mut buffer, &mut data, 128).unwrap(),
            vec!["final payload"]
        );

        let mut data = Vec::new();
        let mut payloads = Vec::new();
        let error = process_sse_line(b"data: \xff", &mut data, 128, &mut payloads).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidResponse);

        let mut buffer = Vec::new();
        let mut data = Vec::new();
        assert_eq!(
            parse_sse(
                &mut buffer,
                &mut data,
                b"data: first\r\ndata: second\r\n\r\n",
                128
            )
            .unwrap(),
            vec!["first\nsecond"]
        );
        let mut buffer = b"data: [DONE]\r".to_vec();
        assert_eq!(
            finish_sse(&mut buffer, &mut data, 128).unwrap(),
            vec!["[DONE]"]
        );
    }

    #[test]
    fn protocol_helper_mappings_cover_all_choice_and_content_variants() {
        let base64 = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "YWJj".into(),
        };
        let url = ImageSource::Url {
            url: "https://example.test/image.png".into(),
        };
        assert_eq!(image_url(&url), "https://example.test/image.png");
        assert_eq!(image_url(&base64), "data:image/png;base64,YWJj");
        assert_eq!(
            chat_image(&base64).pointer("/image_url/url"),
            Some(&json!("data:image/png;base64,YWJj"))
        );
        assert_eq!(
            responses_image(&url).pointer("/image_url"),
            Some(&json!("https://example.test/image.png"))
        );
        assert_eq!(
            claude_messages_image(&base64).pointer("/source/type"),
            Some(&json!("base64"))
        );

        let parts = vec![
            ContentPart::Text {
                text: "first".into(),
            },
            ContentPart::Image {
                source: base64.clone(),
            },
            ContentPart::Text {
                text: " second".into(),
            },
        ];
        assert_eq!(tool_result_text(&parts, false), "first second");
        assert_eq!(tool_result_text(&parts, true), "ERROR: first second");
        assert_eq!(role_name(Role::System), "system");
        assert_eq!(role_name(Role::User), "user");
        assert_eq!(role_name(Role::Assistant), "assistant");
        assert_eq!(role_name(Role::Tool), "tool");

        for (choice, expected_openai, expected_claude) in [
            (ToolChoice::Auto, json!("auto"), json!({"type":"auto"})),
            (ToolChoice::None, json!("none"), json!({"type":"none"})),
            (
                ToolChoice::Required,
                json!("required"),
                json!({"type":"any"}),
            ),
        ] {
            assert_eq!(openai_tool_choice(&choice, false), expected_openai);
            assert_eq!(claude_messages_tool_choice(&choice), expected_claude);
        }
        let named = ToolChoice::Named("read_file".into());
        assert_eq!(
            openai_tool_choice(&named, true),
            json!({"type":"function","name":"read_file"})
        );
        assert_eq!(
            openai_tool_choice(&named, false),
            json!({"type":"function","function":{"name":"read_file"}})
        );
        assert_eq!(
            claude_messages_tool_choice(&named),
            json!({"type":"tool","name":"read_file"})
        );
    }

    #[test]
    fn formats_usage_and_value_parsers_preserve_protocol_specific_fields() {
        let schema = ResponseFormat::JsonSchema {
            name: "result".into(),
            schema: json!({"type":"object"}),
            strict: true,
        };
        assert_eq!(
            openai_response_format(&ResponseFormat::JsonObject, false),
            json!({"type":"json_object"})
        );
        assert_eq!(
            openai_response_format(&schema, true).pointer("/name"),
            Some(&json!("result"))
        );
        assert_eq!(
            openai_response_format(&schema, false).pointer("/json_schema/name"),
            Some(&json!("result"))
        );

        let structured_request = ProviderRequest {
            response_format: Some(ResponseFormat::JsonObject),
            ..ProviderRequest::text("model", "prompt")
        };
        assert_eq!(
            structured_output(
                &json!({"choices":[{"message":{"content":"{\"ok\":true}"}}]}),
                &structured_request
            )
            .unwrap(),
            Some(json!({"ok":true}))
        );
        assert_eq!(
            structured_output(
                &json!({"output":[{"content":[{"text":"{\"ok\":false}"}]}]}),
                &structured_request
            )
            .unwrap(),
            Some(json!({"ok":false}))
        );
        assert_eq!(
            structured_output(
                &json!({"choices":[{"message":{"content":"not json"}}]}),
                &structured_request
            )
            .unwrap_err()
            .kind,
            ErrorKind::InvalidResponse
        );
        assert!(
            structured_output(&json!({}), &ProviderRequest::text("model", "prompt"))
                .unwrap()
                .is_none()
        );

        let openai = openai_usage(Some(&json!({
            "prompt_tokens":2,"completion_tokens":3,
            "prompt_tokens_details":{"cached_tokens":1},
            "completion_tokens_details":{"reasoning_tokens":2}
        })));
        assert_eq!(
            (
                openai.input_tokens,
                openai.output_tokens,
                openai.total_tokens
            ),
            (2, 3, 5)
        );
        assert_eq!(openai.cache_read_input_tokens, Some(1));
        assert_eq!(openai.reasoning_tokens, Some(2));
        let responses = responses_usage(Some(&json!({
            "input_tokens":4,"output_tokens":5,"total_tokens":12,
            "input_tokens_details":{"cached_tokens":3},
            "output_tokens_details":{"reasoning_tokens":4}
        })));
        assert_eq!(
            (
                responses.input_tokens,
                responses.output_tokens,
                responses.total_tokens
            ),
            (4, 5, 12)
        );
        assert_eq!(responses.cache_read_input_tokens, Some(3));
        assert_eq!(responses.reasoning_tokens, Some(4));
        let claude = claude_messages_usage(Some(&json!({
            "input_tokens":6,"output_tokens":7,
            "cache_read_input_tokens":2,"cache_creation_input_tokens":3
        })));
        assert_eq!(
            (
                claude.input_tokens,
                claude.output_tokens,
                claude.total_tokens
            ),
            (6, 7, 13)
        );
        assert_eq!(claude.cache_read_input_tokens, Some(2));
        assert_eq!(claude.cache_write_input_tokens, Some(3));
        assert_eq!(
            openai_usage(Some(&json!({"prompt_tokens":2,"completion_tokens":3})))
                .cache_read_input_tokens,
            None
        );
        assert_eq!(
            responses_usage(Some(&json!({"input_tokens":2,"output_tokens":3})))
                .cache_read_input_tokens,
            None
        );
        assert_eq!(
            claude_messages_usage(Some(&json!({"input_tokens":2,"output_tokens":3})))
                .cache_write_input_tokens,
            None
        );

        assert_eq!(parse_arguments(None).unwrap(), json!({}));
        assert_eq!(
            parse_arguments(Some(&json!("{\"path\":\"a\"}"))).unwrap(),
            json!({"path":"a"})
        );
        assert_eq!(
            parse_arguments(Some(&json!({"path":"a"}))).unwrap(),
            json!({"path":"a"})
        );
        assert_eq!(
            parse_arguments(Some(&json!("{"))).unwrap_err().kind,
            ErrorKind::InvalidRequest
        );
        assert_eq!(
            string_at(&json!({"field":"value"}), "/field").unwrap(),
            "value"
        );
        assert_eq!(
            string_at(&json!({}), "/field").unwrap_err().kind,
            ErrorKind::InvalidRequest
        );
    }

    #[test]
    fn helper_boundaries_cover_images_choices_usage_and_redaction() {
        let url = ImageSource::Url {
            url: "https://example.test/a.png".into(),
        };
        let base64 = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AQI=".into(),
        };
        assert_eq!(image_url(&url), "https://example.test/a.png");
        assert_eq!(image_url(&base64), "data:image/png;base64,AQI=");
        assert_eq!(chat_image(&url)["type"], json!("image_url"));
        assert_eq!(responses_image(&base64)["type"], json!("input_image"));
        assert_eq!(claude_messages_image(&url)["source"]["type"], json!("url"));
        assert_eq!(
            claude_messages_image(&base64)["source"]["type"],
            json!("base64")
        );

        assert_eq!(role_name(Role::System), "system");
        assert_eq!(role_name(Role::Tool), "tool");
        for choice in [ToolChoice::Auto, ToolChoice::None, ToolChoice::Required] {
            assert!(!openai_tool_choice(&choice, false).is_null());
            assert!(!claude_messages_tool_choice(&choice).is_null());
        }
        let named = ToolChoice::Named("echo".into());
        assert_eq!(openai_tool_choice(&named, true)["name"], json!("echo"));
        assert_eq!(
            openai_tool_choice(&named, false)["function"]["name"],
            json!("echo")
        );
        assert_eq!(claude_messages_tool_choice(&named)["name"], json!("echo"));
        assert_eq!(
            tool_result_text(&[ContentPart::Text { text: "bad".into() }], true),
            "ERROR: bad"
        );
        assert_eq!(
            tool_result_text(&[ContentPart::Text { text: "ok".into() }], false),
            "ok"
        );

        let mut usage = Usage::default();
        merge_usage(
            &mut usage,
            Usage {
                input_tokens: 2,
                output_tokens: 3,
                total_tokens: 0,
                cache_read_input_tokens: Some(1),
                cache_write_input_tokens: Some(2),
                reasoning_tokens: Some(4),
            },
        );
        assert_eq!(usage.total_tokens, 5);
        assert_eq!(openai_usage(Some(&json!({"prompt_tokens":2,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":1},"completion_tokens_details":{"reasoning_tokens":4}}))).total_tokens, 5);
        assert_eq!(
            responses_usage(Some(&json!({"input_tokens":2,"output_tokens":3}))).total_tokens,
            5
        );
        assert_eq!(claude_messages_usage(Some(&json!({"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":1,"cache_creation_input_tokens":2}))).total_tokens, 5);
        assert_eq!(
            redact_error_text("  token=secret   value ", "secret"),
            "token=*** value"
        );
    }

    #[test]
    fn stream_event_matrix_covers_tool_and_terminal_variants() {
        let mut chat = StreamAccumulator::new(ProviderKind::OpenAiChatCompletions);
        let events = chat
            .normalize(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"echo","arguments":"{\"x\":"}}]}}]}))
            .unwrap();
        assert!(matches!(
            events[0],
            ProviderStreamEvent::ToolArgumentsDelta { .. }
        ));
        chat.normalize(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"length"}],"usage":{"prompt_tokens":1}})).unwrap();
        assert!(matches!(
            chat.finish(),
            Some(ProviderStreamEvent::Done {
                stop_reason: StopReason::MaxTokens,
                ..
            })
        ));
        assert!(chat.finish().is_none());

        let mut responses = StreamAccumulator::new(ProviderKind::OpenAiResponses);
        responses.normalize(&json!({"type":"response.output_item.added","item":{"type":"function_call","id":"i","call_id":"c","name":"echo"}})).unwrap();
        let events = responses.normalize(&json!({"type":"response.function_call_arguments.delta","item_id":"i","delta":"{}"})).unwrap();
        assert!(matches!(
            events[0],
            ProviderStreamEvent::ToolArgumentsDelta { name: Some(_), .. }
        ));
        let done = responses.normalize(&json!({"type":"response.incomplete","incomplete_details":{"reason":"content_filter"}})).unwrap();
        assert!(matches!(
            done[0],
            ProviderStreamEvent::Done {
                stop_reason: StopReason::ContentFilter,
                ..
            }
        ));

        let mut claude = StreamAccumulator::new(ProviderKind::ClaudeMessages);
        claude.normalize(&json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"echo"}})).unwrap();
        let events = claude.normalize(&json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}})).unwrap();
        assert!(matches!(
            events[0],
            ProviderStreamEvent::ToolArgumentsDelta { .. }
        ));
        assert!(matches!(
            claude
                .normalize(&json!({"type":"content_block_stop","index":0}))
                .unwrap()[0],
            ProviderStreamEvent::BlockFinished { index: 0 }
        ));
        let done = claude.normalize(&json!({"type":"message_delta","delta":{"stop_reason":"stop_sequence"},"usage":{"output_tokens":2}})).unwrap();
        assert!(matches!(
            done[0],
            ProviderStreamEvent::Done {
                stop_reason: StopReason::StopSequence,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn model_discovery_uses_bearer_auth_and_rejects_bad_payloads() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let size = socket.read(&mut buffer).unwrap();
            let request = String::from_utf8_lossy(&buffer[..size]);
            assert!(request.starts_with("GET /v1/models HTTP/1.1"));
            assert_rivo_user_agent(&request);
            assert!(request.contains("authorization: Bearer loopback-key"));
            let body = r#"{"data":[{"id":"test-model"},{"id":"test-model"}]}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let models = discover_models(ProviderConfig {
            base_url: format!("http://{address}/v1"),
            api_key: "loopback-key".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024,
        })
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(models, vec!["test-model"]);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).unwrap();
            let body = r#"{"error":"no models"}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let error = discover_models(ProviderConfig {
            base_url: format!("http://{address}"),
            api_key: "loopback-key".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024,
        })
        .await
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.kind, ErrorKind::InvalidResponse);
    }

    #[tokio::test]
    async fn model_discovery_retries_with_anthropic_auth_after_bearer_rejection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for status in ["401 Unauthorized", "200 OK"] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 4096];
                let size = socket.read(&mut buffer).unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..size]).to_ascii_lowercase());
                let body = if status == "200 OK" {
                    r#"{"data":[{"id":"claude-model"}]}"#
                } else {
                    r#"{"error":{"message":"unauthorized"}}"#
                };
                write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        let models = discover_models(ProviderConfig {
            base_url: format!("http://{address}"),
            api_key: "loopback-key".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024,
        })
        .await
        .unwrap();
        let requests = server.join().unwrap();
        assert_eq!(models, vec!["claude-model"]);
        assert!(requests[0].contains("authorization: bearer loopback-key"));
        assert!(!requests[0].contains("x-api-key:"));
        assert!(requests[1].contains("x-api-key: loopback-key"));
        assert!(requests[1].contains("anthropic-version: 2023-06-01"));
        assert!(!requests[1].contains("authorization:"));
    }

    #[tokio::test]
    async fn anonymous_model_discovery_omits_authentication_headers() {
        for base in ["", "/v1"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 4096];
                let size = socket.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..size]).to_ascii_lowercase();
                assert!(!request.contains("authorization:"));
                assert!(!request.contains("x-api-key:"));
                let body = r#"{"data":[{"id":"anonymous-model"}]}"#;
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            assert_eq!(
                discover_models(ProviderConfig {
                    base_url: format!("http://{address}{base}"),
                    api_key: String::new(),
                    timeout_seconds: 5,
                    max_stream_event_bytes: 1024,
                })
                .await
                .unwrap(),
                vec!["anonymous-model"]
            );
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn non_retriable_http_and_transport_errors_are_returned() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).unwrap();
            let body = r#"{"error":{"message":"bad request"}}"#;
            write!(socket, "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let error = OpenAiChatCompletionsClient::new(ProviderConfig {
            base_url: format!("http://{address}"),
            api_key: "loopback-key".into(),
            timeout_seconds: 5,
            max_stream_event_bytes: 1024,
        })
        .unwrap()
        .complete(ProviderRequest::text("model", "hello"))
        .await
        .unwrap_err();
        server.join().unwrap();
        assert_eq!(error.kind, ErrorKind::InvalidRequest);

        let error = OpenAiResponsesClient::new(ProviderConfig {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "loopback-key".into(),
            timeout_seconds: 1,
            max_stream_event_bytes: 1024,
        })
        .unwrap()
        .complete(ProviderRequest::text("model", "hello"))
        .await
        .unwrap_err();
        assert!(!error.message.is_empty());
    }
}
