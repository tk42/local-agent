///
/// tool_call_stream.rs - Aggregator for streamed tool_calls
///
/// async-openai delivers each tool_call fragment as a `ChatCompletionMessageToolCallChunk`
/// with an explicit `index`. We trust that index and grow a Vec slot-by-slot.
///
/// We do NOT support id-based fallback, "neither index nor id" recovery, or
/// Hermes/Qwen `<tool_call>` tag extraction. If a server doesn't follow the
/// OpenAI streaming spec, fix the server (or its `--jinja` template) — not
/// this aggregator.
///
use serde_json::Value;

use crate::llm_client::ParsedToolCall;

#[derive(Debug, Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
pub struct StreamingToolCallAccumulator {
    entries: Vec<ToolCallAcc>,
}

impl StreamingToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge a single chunk. The OpenAI streaming protocol always supplies
    /// `index`; missing fields just mean "no update for this slot this chunk."
    pub fn ingest(
        &mut self,
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        args_delta: Option<&str>,
    ) {
        let idx = index as usize;
        while self.entries.len() <= idx {
            self.entries.push(ToolCallAcc::default());
        }
        let e = &mut self.entries[idx];
        if let Some(s) = id {
            if !s.is_empty() && e.id.is_empty() {
                e.id = s.to_string();
            }
        }
        if let Some(s) = name {
            if !s.is_empty() && e.name.is_empty() {
                e.name = s.to_string();
            }
        }
        if let Some(s) = args_delta {
            e.arguments.push_str(s);
        }
    }

    /// Convert into ParsedToolCall list. Empty IDs become `call_{N}` so the
    /// follow-up `tool` messages have a stable id to reference.
    pub fn finalize(self) -> Option<Vec<ParsedToolCall>> {
        if self.entries.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(self.entries.len());
        for (i, acc) in self.entries.into_iter().enumerate() {
            let arguments: Value = if acc.arguments.is_empty() {
                Value::Object(serde_json::Map::new())
            } else {
                match serde_json::from_str(&acc.arguments) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "\x1b[33m[warn] tool '{}' arguments not valid JSON ({}); using empty object\x1b[0m",
                            acc.name, e
                        );
                        Value::Object(serde_json::Map::new())
                    }
                }
            };
            let id = if acc.id.is_empty() {
                format!("call_{}", i)
            } else {
                acc.id
            };
            out.push(ParsedToolCall {
                id,
                name: acc.name,
                arguments,
            });
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_chunks_by_index() {
        let mut acc = StreamingToolCallAccumulator::new();
        acc.ingest(0, Some("call_abc"), Some("bash"), Some("{\"comm"));
        acc.ingest(0, None, None, Some("and\":\"ls\"}"));
        let out = acc.finalize().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "call_abc");
        assert_eq!(out[0].name, "bash");
        assert_eq!(out[0].arguments["command"], "ls");
    }

    #[test]
    fn synthesizes_id_when_missing() {
        let mut acc = StreamingToolCallAccumulator::new();
        acc.ingest(0, None, Some("bash"), Some("{}"));
        let out = acc.finalize().unwrap();
        assert_eq!(out[0].id, "call_0");
    }

    #[test]
    fn parallel_calls_keep_separate_indices() {
        let mut acc = StreamingToolCallAccumulator::new();
        acc.ingest(0, Some("a"), Some("read_file"), Some("{\"path\":\"a\"}"));
        acc.ingest(1, Some("b"), Some("read_file"), Some("{\"path\":\"b\"}"));
        let out = acc.finalize().unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].arguments["path"], "a");
        assert_eq!(out[1].arguments["path"], "b");
    }

    #[test]
    fn malformed_arguments_become_empty_object() {
        let mut acc = StreamingToolCallAccumulator::new();
        acc.ingest(0, Some("x"), Some("bash"), Some("{not json"));
        let out = acc.finalize().unwrap();
        assert_eq!(out[0].arguments, json!({}));
    }

    #[test]
    fn out_of_order_indices_grow_slots() {
        let mut acc = StreamingToolCallAccumulator::new();
        // Server sends index 2 first; slots 0 and 1 are empty placeholders
        // that finalize as call_0 / call_1 with empty name. This is rare
        // but defended so we don't panic.
        acc.ingest(2, Some("c"), Some("bash"), Some("{}"));
        let out = acc.finalize().unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].id, "c");
        assert_eq!(out[2].name, "bash");
    }
}
