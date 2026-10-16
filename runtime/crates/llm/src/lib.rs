pub mod computer_use;
mod provider;
#[cfg(test)]
mod release_version;

pub use provider::{
    discover_models, provider_request_url, ClaudeMessagesClient, OpenAiChatCompletionsClient,
    OpenAiResponsesClient,
};

use async_trait::async_trait;
use futures::{stream, Stream, StreamExt};
use rivo_core::{MediaOperation, Message, ToolDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::fmt::{Display, Formatter};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "claude_messages")]
    ClaudeMessages,
}

impl ProviderKind {
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAiChatCompletions | Self::OpenAiResponses => "https://api.openai.com/v1",
            Self::ClaudeMessages => "https://api.anthropic.com",
        }
    }

    pub fn default_api_key_env(self) -> &'static str {
        match self {
            Self::OpenAiChatCompletions | Self::OpenAiResponses => "OPENAI_API_KEY",
            Self::ClaudeMessages => "ANTHROPIC_API_KEY",
        }
    }
}

impl Display for ProviderKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::OpenAiChatCompletions => "openai_chat_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::ClaudeMessages => "claude_messages",
        })
    }
}

impl FromStr for ProviderKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai_chat_completions" => Ok(Self::OpenAiChatCompletions),
            "openai_responses" => Ok(Self::OpenAiResponses),
            "claude_messages" => Ok(Self::ClaudeMessages),
            _ => Err(format!("不支持的 provider: {value}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    JsonObject,
    JsonSchema {
        name: String,
        schema: Value,
        strict: bool,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    #[default]
    #[serde(rename = "default", alias = "provider_default")]
    ProviderDefault,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderDefault => "default",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
    pub tool_choice: ToolChoice,
    pub response_format: Option<ResponseFormat>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    #[serde(default)]
    pub metadata: std::collections::BTreeMap<String, String>,
}

impl ProviderRequest {
    pub fn text(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: None,
            messages: vec![Message::text(rivo_core::Role::User, prompt)],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            response_format: None,
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: ReasoningEffort::default(),
            metadata: Default::default(),
        }
    }

    /// Enable a provider's native Computer Use tool for this request. The
    /// flag is carried in metadata so existing request constructors remain
    /// source-compatible; provider adapters consume it and never send the
    /// internal metadata key over the wire.
    pub fn enable_computer_use(&mut self) {
        self.metadata
            .insert("rivo.computer_use".into(), "true".into());
    }

    pub fn computer_use_enabled(&self) -> bool {
        self.metadata.get("rivo.computer_use").map(String::as_str) == Some("true")
    }

    /// Some OpenAI-compatible gateways implement Responses function tools but
    /// reject the native `computer` tool declaration. Keep this opt-in so the
    /// native OpenAI wire contract remains the default.
    pub fn computer_use_function_fallback(&self) -> bool {
        self.metadata
            .get("rivo.computer_use_function_fallback")
            .map(String::as_str)
            == Some("true")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Final,
    ToolUse,
    MaxTokens,
    StopSequence,
    ContentFilter,
    Error,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// 把另一次模型调用的用量累加进来；可选计数只要任一方上报就保留。
    pub fn accumulate(&mut self, other: &Usage) {
        fn add(left: Option<u64>, right: Option<u64>) -> Option<u64> {
            match (left, right) {
                (None, None) => None,
                (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
            }
        }
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cache_read_input_tokens =
            add(self.cache_read_input_tokens, other.cache_read_input_tokens);
        self.cache_write_input_tokens = add(
            self.cache_write_input_tokens,
            other.cache_write_input_tokens,
        );
        self.reasoning_tokens = add(self.reasoning_tokens, other.reasoning_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderResponse {
    pub message: Message,
    pub structured_output: Option<Value>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub provider_raw: Option<Value>,
    pub request_id: Option<String>,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Auth,
    InvalidRequest,
    RateLimit,
    Server,
    Timeout,
    Network,
    ContentPolicy,
    ContextLength,
    InvalidResponse,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub retriable: bool,
    pub http_status: Option<u16>,
    pub provider_code: Option<String>,
    pub message: String,
}

impl Display for ProviderError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} ({:?}", self.message, self.kind)?;
        if let Some(status) = self.http_status {
            write!(formatter, ", HTTP {status}")?;
        }
        if let Some(code) = &self.provider_code {
            write!(formatter, ", provider code {code}")?;
        }
        write!(formatter, ")")
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidRequest,
            retriable: false,
            http_status: None,
            provider_code: None,
            message: message.into(),
        }
    }

    pub fn invalid_response(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidResponse,
            retriable: false,
            http_status: None,
            provider_code: None,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderStreamEvent {
    TextDelta {
        delta: String,
    },
    ToolArgumentsDelta {
        call_id: String,
        name: Option<String>,
        delta: String,
    },
    BlockStarted {
        index: usize,
    },
    BlockFinished {
        index: usize,
    },
    Done {
        stop_reason: StopReason,
        usage: Usage,
    },
}

pub type ProviderStream =
    Pin<Box<dyn Stream<Item = Result<ProviderStreamEvent, ProviderError>> + Send>>;

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError>;
    fn stream(&self, request: ProviderRequest) -> ProviderStream;
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub timeout_seconds: u64,
    pub max_stream_event_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInput {
    pub media_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaProviderRequest {
    pub model: String,
    pub operation: MediaOperation,
    pub prompt: String,
    pub inputs: Vec<MediaInput>,
    pub size: Option<String>,
    pub output_media_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaProviderResponse {
    pub data: Vec<u8>,
    pub media_type: String,
}

#[async_trait]
pub trait MediaClient: Send + Sync {
    async fn execute_media(
        &self,
        request: MediaProviderRequest,
    ) -> Result<MediaProviderResponse, ProviderError>;
}

pub fn create_media_client(
    kind: ProviderKind,
    config: ProviderConfig,
) -> Result<Arc<dyn MediaClient>, ProviderError> {
    provider::create_media_client(kind, config)
}

impl ProviderConfig {
    pub fn new(kind: ProviderKind, api_key: impl Into<String>) -> Self {
        Self {
            base_url: kind.default_base_url().into(),
            api_key: api_key.into(),
            timeout_seconds: 120,
            max_stream_event_bytes: 1024 * 1024,
        }
    }
}

pub fn create_client(
    kind: ProviderKind,
    config: ProviderConfig,
) -> Result<Arc<dyn LlmClient>, ProviderError> {
    match kind {
        ProviderKind::OpenAiChatCompletions => {
            Ok(Arc::new(OpenAiChatCompletionsClient::new(config)?))
        }
        ProviderKind::OpenAiResponses => Ok(Arc::new(OpenAiResponsesClient::new(config)?)),
        ProviderKind::ClaudeMessages => Ok(Arc::new(ClaudeMessagesClient::new(config)?)),
    }
}

#[derive(Clone, Default)]
pub struct MockLlm {
    responses: Arc<Mutex<VecDeque<Result<ProviderResponse, ProviderError>>>>,
}

impl MockLlm {
    pub fn from_responses(
        responses: impl IntoIterator<Item = Result<ProviderResponse, ProviderError>>,
    ) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
        }
    }
}

#[async_trait]
impl LlmClient for MockLlm {
    async fn complete(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.responses
            .lock()
            .expect("mock response lock poisoned")
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::invalid("MockLlm 没有更多预设响应")))
    }

    fn stream(&self, request: ProviderRequest) -> ProviderStream {
        let this = self.clone();
        Box::pin(
            stream::once(async move { this.complete(request).await }).flat_map(|response| {
                let events = match response {
                    Ok(response) => {
                        let mut events = Vec::new();
                        for part in &response.message.content {
                            match part {
                                rivo_core::ContentPart::Text { text } => {
                                    events.push(Ok(ProviderStreamEvent::TextDelta {
                                        delta: text.clone(),
                                    }))
                                }
                                rivo_core::ContentPart::ToolUse {
                                    id,
                                    name,
                                    arguments,
                                } => events.push(Ok(ProviderStreamEvent::ToolArgumentsDelta {
                                    call_id: id.clone(),
                                    name: Some(name.clone()),
                                    delta: arguments.to_string(),
                                })),
                                _ => {}
                            }
                        }
                        events.push(Ok(ProviderStreamEvent::Done {
                            stop_reason: response.stop_reason,
                            usage: response.usage,
                        }));
                        events
                    }
                    Err(error) => vec![Err(error)],
                };
                stream::iter(events)
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use rivo_core::Role;

    fn response(text: &str) -> ProviderResponse {
        ProviderResponse {
            message: Message::text(Role::Assistant, text),
            structured_output: None,
            stop_reason: StopReason::Final,
            usage: Usage::default(),
            provider_raw: None,
            request_id: None,
            model: "mock".into(),
        }
    }

    #[tokio::test]
    async fn mock_is_deterministic_for_complete_and_stream() {
        let mock = MockLlm::from_responses([Ok(response("one")), Ok(response("two"))]);
        assert_eq!(
            mock.complete(ProviderRequest::text("mock", "hello"))
                .await
                .unwrap()
                .message
                .text_content(),
            "one"
        );
        let events = mock
            .stream(ProviderRequest::text("mock", "hello"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            events[0].as_ref().unwrap(),
            &ProviderStreamEvent::TextDelta {
                delta: "two".into()
            }
        );
    }

    #[test]
    fn provider_kind_accepts_only_standard_protocol_names() {
        for name in [
            "openai_chat_completions",
            "openai_responses",
            "claude_messages",
        ] {
            assert!(name.parse::<ProviderKind>().is_ok());
        }
        for legacy in [
            "chat",
            "chat_completions",
            "openai_compatible",
            "responses",
            "anthropic",
            "messages",
            "claudemessage",
            "claude-message",
        ] {
            assert!(legacy.parse::<ProviderKind>().is_err());
        }
    }

    #[tokio::test]
    async fn mock_exhaustion_and_error_stream_are_explicit() {
        let mock = MockLlm::default();
        let error = mock
            .complete(ProviderRequest::text("mock", "hello"))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
        assert!(error.message.contains("没有更多"));

        let expected = ProviderError::invalid("fixture failure");
        let mock = MockLlm::from_responses([Err(expected.clone())]);
        let events = mock
            .stream(ProviderRequest::text("mock", "hello"))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events, vec![Err(expected)]);
    }

    #[test]
    fn provider_defaults_and_client_factory_cover_all_standard_protocols() {
        let cases = [
            (
                ProviderKind::OpenAiChatCompletions,
                "https://api.openai.com/v1",
                "OPENAI_API_KEY",
            ),
            (
                ProviderKind::OpenAiResponses,
                "https://api.openai.com/v1",
                "OPENAI_API_KEY",
            ),
            (
                ProviderKind::ClaudeMessages,
                "https://api.anthropic.com",
                "ANTHROPIC_API_KEY",
            ),
        ];
        for (kind, base_url, env) in cases {
            assert_eq!(kind.default_base_url(), base_url);
            assert_eq!(kind.default_api_key_env(), env);
            let config = ProviderConfig::new(kind, "test-key");
            assert_eq!(config.base_url, base_url);
            assert_eq!(config.timeout_seconds, 120);
            assert!(create_client(kind, config).is_ok());
        }
        let display = ProviderKind::OpenAiResponses.to_string();
        assert_eq!(display, "openai_responses");
    }
}
