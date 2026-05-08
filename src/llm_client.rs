///
/// llm_client.rs - Thin reqwest-based client for llama-server / OpenAI-
/// compatible chat completion endpoints.
///
/// We talk to the wire format directly: SSE via `reqwest-eventsource`, JSON
/// chunks via `serde_json::Value`. This keeps us insulated from upstream SDK
/// bugs and the schema variations llama-server / vLLM / etc. introduce. The
/// only things we own beyond the HTTP/SSE layer are the retry loop and tool-
/// call aggregation (see `tool_call_stream.rs`).
///
use std::io::{self, Write};

use anyhow::{bail, Result};
use futures_util::StreamExt;
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool_call_stream::StreamingToolCallAccumulator;

// ---------------------------------------------------------------------------
// Public types — kept stable so the rest of the crate is unaffected.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    // `content` is intentionally NOT skipped when None: llama-server / OpenAI
    // expect the field present (as `null` for tool-call-only assistant turns,
    // or as an empty string elsewhere — see `agent_loop` in main.rs).
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
    pub context_tokens: u32,
    pub temperature: f64,
}

impl LlmConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self {
            base_url: std::env::var("LLM_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8080/v1".into()),
            api_key: std::env::var("LLM_API_KEY")
                .unwrap_or_else(|_| "sk-no-key-required".into()),
            model: std::env::var("LLM_MODEL").unwrap_or_else(|_| "any-model-name".into()),
            max_tokens: std::env::var("LLM_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8192),
            context_tokens: std::env::var("LLM_CONTEXT_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(65536),
            temperature: std::env::var("LLM_TEMPERATURE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.6),
        };
        cfg.reclamp();
        cfg
    }

    /// Ensure `max_tokens` leaves room for prompt within `context_tokens`.
    /// llama-server rejects requests where prompt + max_tokens > n_ctx, so cap
    /// max_tokens at n_ctx/8 when it grows past 7/8 of the window.
    pub fn reclamp(&mut self) {
        if self.context_tokens == 0 {
            return;
        }
        let limit = self.context_tokens.saturating_mul(7) / 8;
        if self.max_tokens >= limit {
            let clamped = (self.context_tokens / 8).max(256);
            eprintln!(
                "\x1b[33m[warn] LLM_MAX_TOKENS={} >= LLM_CONTEXT_TOKENS={}; clamping to {}\x1b[0m",
                self.max_tokens, self.context_tokens, clamped
            );
            self.max_tokens = clamped;
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct LlmClient {
    pub config: LlmConfig,
    http: Client,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { config, http }
    }

    pub fn set_context_tokens(&mut self, n: u32) {
        self.config.context_tokens = n;
        self.config.reclamp();
    }

    pub fn set_max_tokens(&mut self, n: u32) {
        self.config.max_tokens = n;
        self.config.reclamp();
    }

    /// Streaming chat completion with tool support.
    pub async fn chat(&self, messages: &[Message], tools: Option<&[Value]>) -> Result<ChatResult> {
        let body = build_chat_body(&self.config, messages, tools, true);
        for attempt in 0..3u32 {
            match self.stream_chat(&body).await {
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
        let body = serde_json::json!({
            "model": self.config.model,
            "messages": [{
                "role": "user",
                "content": format!("Summarize the following conversation for continuity:\n\n{}", text)
            }],
            "max_tokens": max_tokens,
            "temperature": 0.3,
            "stream": false,
        });
        let url = format!("{}/chat/completions", self.config.base_url);
        let resp: Value = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("(empty summary)")
            .to_string())
    }

    async fn stream_chat(&self, body: &Value) -> Result<ChatResult> {
        let url = format!("{}/chat/completions", self.config.base_url);
        let req = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(body)?);
        let mut es = EventSource::new(req)?;

        let mut content_buffer = String::new();
        let mut acc = StreamingToolCallAccumulator::new();
        let mut tool_calls_started = false;
        let mut printed_anything = false;
        let mut finish_reason = String::from("stop");

        while let Some(event) = es.next().await {
            match event {
                Ok(Event::Open) => {}
                Ok(Event::Message(msg)) => {
                    // [DONE] is the OpenAI-spec terminator. Detect it at the
                    // SSE layer — never feed it to a JSON parser.
                    if msg.data.trim() == "[DONE]" {
                        break;
                    }

                    // Non-JSON frames (keepalives, blank `data:` lines, server-
                    // proprietary status pings, partial frames recovered by the
                    // SSE buffer) are skipped silently. The next valid frame
                    // resumes the stream.
                    let chunk: Value = match serde_json::from_str(&msg.data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let Some(choices) = chunk["choices"].as_array() else {
                        continue;
                    };
                    let Some(choice) = choices.first() else {
                        continue;
                    };

                    if let Some(fr) = choice["finish_reason"].as_str() {
                        finish_reason = fr.to_string();
                    }
                    let delta = &choice["delta"];

                    if let Some(text) = delta["content"].as_str() {
                        if !text.is_empty() {
                            content_buffer.push_str(text);
                            if !tool_calls_started {
                                print!("{}", text);
                                io::stdout().flush().ok();
                                printed_anything = true;
                            }
                        }
                    }

                    if let Some(tcs) = delta["tool_calls"].as_array() {
                        for tc in tcs {
                            let index = tc["index"].as_u64().unwrap_or(0) as u32;
                            let id = tc["id"].as_str();
                            let name = tc["function"]["name"].as_str();
                            let args = tc["function"]["arguments"].as_str();
                            acc.ingest(index, id, name, args);
                        }
                        tool_calls_started = true;
                    }
                }
                // Server closed without emitting `[DONE]` (e.g. llama-server
                // sometimes does this on hitting max_tokens) — treat as end.
                Err(reqwest_eventsource::Error::StreamEnded) => break,
                Err(e) => {
                    let prompt_tok_est = serde_json::to_string(body)
                        .map(|s| s.len() / 4)
                        .unwrap_or(0);
                    bail!(
                        "SSE stream error: {} (prompt≈{} tok, max_tokens={}, n_ctx={} — server may have rejected oversized prompt; try /clear, /compact, or lower /maxtokens)",
                        e,
                        prompt_tok_est,
                        self.config.max_tokens,
                        self.config.context_tokens
                    );
                }
            }
        }
        es.close();

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
// Request body
// ---------------------------------------------------------------------------

fn build_chat_body(
    cfg: &LlmConfig,
    messages: &[Message],
    tools: Option<&[Value]>,
    stream: bool,
) -> Value {
    let mut body = serde_json::json!({
        "model": cfg.model,
        "messages": messages,
        "max_tokens": cfg.max_tokens,
        "temperature": cfg.temperature,
        "stream": stream,
    });
    if let Some(ts) = tools {
        if !ts.is_empty() {
            body["tools"] = Value::Array(ts.to_vec());
            body["tool_choice"] = Value::String("auto".into());
        }
    }
    body
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn is_connect_error(e: &anyhow::Error) -> bool {
    let mut cur: Option<&dyn std::error::Error> = Some(e.as_ref());
    while let Some(err) = cur {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod message_wire_tests {
    use super::*;

    #[test]
    fn user_message_shape() {
        let m = Message::user("hi");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"], "hi");
        assert!(v.get("tool_calls").is_none());
        assert!(v.get("tool_call_id").is_none());
    }

    #[test]
    fn assistant_with_tool_calls_serializes_null_content() {
        // OpenAI spec: assistant turns with tool_calls and no text emit
        // `"content": null`. Server-side deserializers expect the field
        // present (commit 12cb0c2 — empty string fallback in main.rs).
        let m = Message::assistant(
            None,
            Some(vec![MessageToolCall {
                id: "call_1".into(),
                call_type: "function".into(),
                function: FunctionCallSerde {
                    name: "read_file".into(),
                    arguments: "{}".into(),
                },
            }]),
        );
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert!(v["content"].is_null());
        assert_eq!(v["tool_calls"][0]["id"], "call_1");
        assert_eq!(v["tool_calls"][0]["type"], "function");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn tool_message_shape() {
        let m = Message::tool("call_1", "ok");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_1");
        assert_eq!(v["content"], "ok");
        assert!(v.get("tool_calls").is_none());
    }

    #[test]
    fn build_chat_body_omits_tools_when_none() {
        let cfg = LlmConfig {
            base_url: "x".into(),
            api_key: "x".into(),
            model: "m".into(),
            max_tokens: 10,
            context_tokens: 65536,
            temperature: 0.5,
        };
        let msgs = vec![Message::user("hi")];
        let body = build_chat_body(&cfg, &msgs, None, true);
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn build_chat_body_includes_tools_when_present() {
        let cfg = LlmConfig {
            base_url: "x".into(),
            api_key: "x".into(),
            model: "m".into(),
            max_tokens: 10,
            context_tokens: 65536,
            temperature: 0.5,
        };
        let msgs = vec![Message::user("hi")];
        let tools = vec![serde_json::json!({"type": "function", "function": {"name": "f"}})];
        let body = build_chat_body(&cfg, &msgs, Some(&tools), true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["function"]["name"], "f");
    }
}
