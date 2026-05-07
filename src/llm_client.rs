///
/// llm_client.rs - OpenAI SDK wrapper for llama-server
///
/// Connects to a local llama-server (or any OpenAI-compatible endpoint)
/// via reqwest. Handles tool-calling response parsing and SSE streaming.
///
use std::collections::BTreeMap;
use std::io::{self, Write};

use anyhow::{bail, Result};
use futures_util::StreamExt;
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
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
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Parsed tool call (after JSON decode)
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub arguments_raw: String,
}

/// Result from a chat completion
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
    http: Client,
}

impl LlmClient {
    pub fn new(config: LlmConfig) -> Self {
        Self {
            config,
            http: Client::new(),
        }
    }

    /// Send a chat completion request with streaming.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[Value]>,
    ) -> Result<ChatResult> {
        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
            "stream": true,
        });

        if let Some(tools) = tools {
            if !tools.is_empty() {
                body["tools"] = Value::Array(tools.to_vec());
                body["tool_choice"] = Value::String("auto".into());
            }
        }

        for attempt in 0..3u32 {
            match self.stream_chat(&body).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if is_connection_error(&e) {
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

    /// Summarize text for context compression.
    pub async fn summarize(&self, text: &str, max_tokens: u32) -> Result<String> {
        let body = serde_json::json!({
            "model": self.config.model,
            "messages": [
                {"role": "user", "content": format!("Summarize the following conversation for continuity:\n\n{}", text)}
            ],
            "max_tokens": max_tokens,
            "temperature": 0.3,
            "stream": false,
        });

        let url = format!("{}/chat/completions", self.config.base_url);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&body)
            .send()
            .await?
            .json::<Value>()
            .await?;

        let content = resp["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("(empty summary)");
        Ok(content.to_string())
    }

    async fn stream_chat(&self, body: &Value) -> Result<ChatResult> {
        let url = format!("{}/chat/completions", self.config.base_url);

        let request = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(body)?);

        let mut es = EventSource::new(request)?;

        let mut content_parts: Vec<String> = Vec::new();
        let mut tool_calls_acc: BTreeMap<u64, ToolCallAcc> = BTreeMap::new();
        let mut finish_reason = String::from("stop");

        while let Some(event) = es.next().await {
            match event {
                Ok(Event::Open) => {}
                Ok(Event::Message(msg)) => {
                    if msg.data == "[DONE]" {
                        break;
                    }
                    let chunk: Value = match serde_json::from_str(&msg.data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let choices = match chunk["choices"].as_array() {
                        Some(c) => c,
                        None => continue,
                    };
                    if choices.is_empty() {
                        continue;
                    }

                    let choice = &choices[0];
                    if let Some(fr) = choice["finish_reason"].as_str() {
                        finish_reason = fr.to_string();
                    }

                    let delta = &choice["delta"];

                    // Text content
                    if let Some(text) = delta["content"].as_str() {
                        print!("{}", text);
                        io::stdout().flush().ok();
                        content_parts.push(text.to_string());
                    }

                    // Tool calls (streamed incrementally)
                    if let Some(tcs) = delta["tool_calls"].as_array() {
                        for tc in tcs {
                            let idx = tc["index"].as_u64().unwrap_or(0);
                            let entry = tool_calls_acc.entry(idx).or_insert_with(|| ToolCallAcc {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });
                            if let Some(id) = tc["id"].as_str() {
                                if !id.is_empty() {
                                    entry.id = id.to_string();
                                }
                            }
                            if let Some(name) = tc["function"]["name"].as_str() {
                                if !name.is_empty() {
                                    entry.name = name.to_string();
                                }
                            }
                            if let Some(args) = tc["function"]["arguments"].as_str() {
                                entry.arguments.push_str(args);
                            }
                        }
                    }
                }
                Err(reqwest_eventsource::Error::StreamEnded) => break,
                Err(e) => {
                    bail!("SSE stream error: {}", e);
                }
            }
        }
        es.close();

        // Newline after streamed text
        if !content_parts.is_empty() {
            println!();
        }

        let content = if content_parts.is_empty() {
            None
        } else {
            Some(content_parts.join(""))
        };

        let tool_calls = if tool_calls_acc.is_empty() {
            None
        } else {
            let mut parsed = Vec::new();
            for (_idx, acc) in &tool_calls_acc {
                let arguments: Value = if acc.arguments.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&acc.arguments).unwrap_or_else(|_| {
                        serde_json::json!({"_raw": acc.arguments})
                    })
                };
                parsed.push(ParsedToolCall {
                    id: acc.id.clone(),
                    name: acc.name.clone(),
                    arguments,
                    arguments_raw: acc.arguments.clone(),
                });
            }
            Some(parsed)
        };

        Ok(ChatResult {
            content,
            tool_calls,
            finish_reason,
        })
    }
}

struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

fn is_connection_error(e: &anyhow::Error) -> bool {
    let s = format!("{:?}", e);
    s.contains("Connection") || s.contains("ConnectError")
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
