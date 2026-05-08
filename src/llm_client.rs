///
/// llm_client.rs - Thin wrapper over async-openai for llama-server / OpenAI-
/// compatible servers.
///
/// We keep our own flat `Message` struct for transcript output and history
/// management, and convert to/from async-openai's typed enums at the API
/// boundary. async-openai owns the HTTP client, SSE parsing, and request
/// shaping; we own retries and tool_call aggregation.
///
use std::io::{self, Write};

use anyhow::{bail, Result};
use async_openai::{
    config::OpenAIConfig,
    error::OpenAIError,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestAssistantMessage, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestToolMessage,
        ChatCompletionRequestToolMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent, ChatCompletionTool, ChatCompletionTools,
        CreateChatCompletionRequestArgs, FunctionCall,
    },
    Client,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool_call_stream::StreamingToolCallAccumulator;

// ---------------------------------------------------------------------------
// Public types — kept stable so the rest of the crate is unaffected.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<MessageToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(content: &str) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn system(content: &str) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<MessageToolCall>>) -> Self {
        Self {
            role: "assistant".into(),
            content,
            tool_calls,
            tool_call_id: None,
        }
    }
    pub fn tool(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCallSerde,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCallSerde {
    pub name: String,
    pub arguments: String,
}

/// Parsed tool call (after JSON decode of arguments)
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug)]
pub struct ChatResult {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ParsedToolCall>>,
    #[allow(dead_code)]
    pub finish_reason: String,
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f64,
}

impl LlmConfig {
    pub fn from_env() -> Self {
        Self {
            base_url: std::env::var("LLM_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8080/v1".into()),
            api_key: std::env::var("LLM_API_KEY")
                .unwrap_or_else(|_| "sk-no-key-required".into()),
            model: std::env::var("LLM_MODEL").unwrap_or_else(|_| "any-model-name".into()),
            max_tokens: std::env::var("LLM_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(32768),
            temperature: std::env::var("LLM_TEMPERATURE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.6),
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct LlmClient {
    pub config: LlmConfig,
    inner: Client<OpenAIConfig>,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Self {
        let oai_cfg = OpenAIConfig::new()
            .with_api_base(config.base_url.clone())
            .with_api_key(config.api_key.clone());

        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let inner = Client::with_config(oai_cfg).with_http_client(http);
        Self { config, inner }
    }

    /// Streaming chat completion with tool support.
    pub async fn chat(&self, messages: &[Message], tools: Option<&[Value]>) -> Result<ChatResult> {
        for attempt in 0..3u32 {
            match self.stream_chat(messages, tools).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if is_connect_error(&e) {
                        die_connection_error(&self.config.base_url);
                    }
                    if attempt < 2 {
                        eprintln!("\x1b[31m[LLM retry {}] {}\x1b[0m", attempt + 1, e);
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!()
    }

    /// Non-streaming summary used by context compression.
    pub async fn summarize(&self, text: &str, max_tokens: u32) -> Result<String> {
        let user_msg: ChatCompletionRequestMessage = ChatCompletionRequestUserMessage {
            content: ChatCompletionRequestUserMessageContent::Text(format!(
                "Summarize the following conversation for continuity:\n\n{}",
                text
            )),
            name: None,
        }
        .into();

        let req = CreateChatCompletionRequestArgs::default()
            .model(self.config.model.clone())
            .messages(vec![user_msg])
            .max_tokens(max_tokens)
            .temperature(0.3_f32)
            .build()?;

        let resp = self.inner.chat().create(req).await?;
        let content = resp
            .choices
            .first()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_else(|| "(empty summary)".to_string());
        Ok(content)
    }

    async fn stream_chat(
        &self,
        messages: &[Message],
        tools: Option<&[Value]>,
    ) -> Result<ChatResult> {
        let oai_messages = messages
            .iter()
            .map(to_openai_message)
            .collect::<Result<Vec<_>>>()?;

        let mut builder = CreateChatCompletionRequestArgs::default();
        builder
            .model(self.config.model.clone())
            .messages(oai_messages)
            .max_tokens(self.config.max_tokens)
            .temperature(self.config.temperature as f32);

        if let Some(tool_values) = tools {
            if !tool_values.is_empty() {
                let tool_objs: Vec<ChatCompletionTools> = tool_values
                    .iter()
                    .map(|v| serde_json::from_value::<ChatCompletionTool>(v.clone()).map(ChatCompletionTools::Function))
                    .collect::<Result<_, _>>()?;
                builder.tools(tool_objs);
            }
        }

        let request = builder.build()?;

        let mut stream = self.inner.chat().create_stream(request).await?;

        let mut content_buffer = String::new();
        let mut acc = StreamingToolCallAccumulator::new();
        let mut tool_calls_started = false;
        let mut printed_anything = false;
        let mut finish_reason = String::from("stop");

        while let Some(item) = stream.next().await {
            let response = item?;
            for choice in &response.choices {
                if let Some(fr) = choice.finish_reason {
                    finish_reason = format!("{:?}", fr).to_lowercase();
                    // Debug repr is e.g. "Stop", "ToolCalls", "Length", "ContentFilter".
                    // Normalize to OpenAI wire spec.
                    finish_reason = match finish_reason.as_str() {
                        "stop" => "stop".into(),
                        "toolcalls" => "tool_calls".into(),
                        "length" => "length".into(),
                        "contentfilter" => "content_filter".into(),
                        "functioncall" => "function_call".into(),
                        other => other.to_string(),
                    };
                }

                if let Some(text) = &choice.delta.content {
                    if !text.is_empty() {
                        content_buffer.push_str(text);
                        if !tool_calls_started {
                            print!("{}", text);
                            io::stdout().flush().ok();
                            printed_anything = true;
                        }
                    }
                }

                if let Some(tcs) = &choice.delta.tool_calls {
                    for tc in tcs {
                        let name = tc.function.as_ref().and_then(|f| f.name.as_deref());
                        let args = tc.function.as_ref().and_then(|f| f.arguments.as_deref());
                        acc.ingest(tc.index, tc.id.as_deref(), name, args);
                    }
                    tool_calls_started = true;
                }
            }
        }

        if printed_anything {
            println!();
        }

        let content = if content_buffer.is_empty() {
            None
        } else {
            Some(content_buffer)
        };
        let tool_calls = acc.finalize();

        Ok(ChatResult {
            content,
            tool_calls,
            finish_reason,
        })
    }
}

// ---------------------------------------------------------------------------
// Conversion: our flat Message → async-openai's typed enum
// ---------------------------------------------------------------------------

fn to_openai_message(msg: &Message) -> Result<ChatCompletionRequestMessage> {
    match msg.role.as_str() {
        "system" => Ok(ChatCompletionRequestSystemMessage {
            content: ChatCompletionRequestSystemMessageContent::Text(
                msg.content.clone().unwrap_or_default(),
            ),
            name: None,
        }
        .into()),
        "user" => Ok(ChatCompletionRequestUserMessage {
            content: ChatCompletionRequestUserMessageContent::Text(
                msg.content.clone().unwrap_or_default(),
            ),
            name: None,
        }
        .into()),
        "assistant" => {
            let tool_calls: Option<Vec<ChatCompletionMessageToolCalls>> =
                msg.tool_calls.as_ref().map(|tcs| {
                    tcs.iter()
                        .map(|tc| {
                            ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                                id: tc.id.clone(),
                                function: FunctionCall {
                                    name: tc.function.name.clone(),
                                    arguments: tc.function.arguments.clone(),
                                },
                            })
                        })
                        .collect()
                });
            #[allow(deprecated)]
            let assistant_msg = ChatCompletionRequestAssistantMessage {
                content: Some(ChatCompletionRequestAssistantMessageContent::Text(
                    msg.content.clone().unwrap_or_default(),
                )),
                refusal: None,
                name: None,
                audio: None,
                tool_calls,
                function_call: None,
            };
            Ok(assistant_msg.into())
        }
        "tool" => Ok(ChatCompletionRequestToolMessage {
            content: ChatCompletionRequestToolMessageContent::Text(
                msg.content.clone().unwrap_or_default(),
            ),
            tool_call_id: msg.tool_call_id.clone().unwrap_or_default(),
        }
        .into()),
        other => bail!("unsupported message role: {}", other),
    }
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

/// Hard connection failure (DNS, refused, etc). For these, retrying with the
/// same body never helps, so we surface immediately.
fn is_connect_error(e: &anyhow::Error) -> bool {
    let mut cur: Option<&dyn std::error::Error> = Some(e.as_ref());
    while let Some(err) = cur {
        if let Some(oai) = err.downcast_ref::<OpenAIError>() {
            if let OpenAIError::Reqwest(re) = oai {
                if re.is_connect() {
                    return true;
                }
            }
        }
        if let Some(re) = err.downcast_ref::<reqwest::Error>() {
            if re.is_connect() {
                return true;
            }
        }
        cur = err.source();
    }
    false
}

fn die_connection_error(base_url: &str) -> ! {
    eprintln!(
        "\n\x1b[31;1m[Error] llama-server に接続できません: {}\x1b[0m\n\
         llama-server が起動しているか確認してください:\n  \
         ./apps/scripts/start-llama-server.sh\n",
        base_url
    );
    std::process::exit(1);
}
