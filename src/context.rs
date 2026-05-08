///
/// context.rs - Context compression utilities
///
/// Prevents context overflow in long sessions via:
/// 1. microcompact: Truncate old tool results to "[cleared]"
/// 2. auto_compact: Summarize the entire conversation and restart with summary
/// 3. estimate_tokens: Rough token count from JSON length
///
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::llm_client::{LlmClient, Message};

/// Auto-compact threshold sized to a fraction of llama-server's `n_ctx`.
/// 60% leaves headroom for `max_tokens`, system prompt, and the next tool
/// result that will be appended after compaction.
pub fn token_threshold(context_tokens: u32) -> usize {
    (context_tokens as usize) * 6 / 10
}

fn transcript_dir() -> PathBuf {
    let dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".transcripts");
    dir
}

/// Rough estimate: ~4 chars per token.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    let json = serde_json::to_string(messages).unwrap_or_default();
    json.len() / 4
}

/// Clear old tool result content to save space.
/// Normally keeps the 3 most recent tool results intact; when `aggressive`
/// is true (caller is near the token threshold), keeps only the most recent 1.
///
/// Also clears the matching assistant tool_call.arguments (by id) so we don't
/// keep paying for huge argument blobs when the corresponding result is gone.
/// IDs are preserved on both sides — that integrity is what OpenAI-compatible
/// servers validate.
pub fn microcompact(messages: &mut [Message], aggressive: bool) {
    let tool_pairs: Vec<(usize, String)> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            if m.role == "tool" {
                m.tool_call_id.clone().map(|id| (i, id))
            } else {
                None
            }
        })
        .collect();

    let keep = if aggressive { 1 } else { 3 };
    if tool_pairs.len() <= keep {
        return;
    }

    let cutoff = tool_pairs.len() - keep;
    let ids_to_clear: std::collections::HashSet<String> = tool_pairs[..cutoff]
        .iter()
        .map(|(_, id)| id.clone())
        .collect();

    for (idx, _) in &tool_pairs[..cutoff] {
        if let Some(ref content) = messages[*idx].content {
            if content.len() > 500 {
                messages[*idx].content = Some("[cleared]".into());
            }
        }
    }

    for msg in messages.iter_mut() {
        if msg.role != "assistant" {
            continue;
        }
        if let Some(ref mut tcs) = msg.tool_calls {
            for tc in tcs.iter_mut() {
                if ids_to_clear.contains(&tc.id) && tc.function.arguments.len() > 200 {
                    tc.function.arguments = "{}".into();
                }
            }
        }
    }
}

/// Save transcript to disk, then summarize and return a fresh 2-message context.
pub async fn auto_compact(client: &LlmClient, messages: &[Message]) -> Result<Vec<Message>> {
    let dir = transcript_dir();
    fs::create_dir_all(&dir)?;

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("transcript_{}.jsonl", ts));

    let mut file_content = String::new();
    for msg in messages {
        let line = serde_json::to_string(msg).unwrap_or_default();
        file_content.push_str(&line);
        file_content.push('\n');
    }
    fs::write(&path, &file_content)?;

    let conv_text = serde_json::to_string(messages).unwrap_or_default();
    let truncated = if conv_text.len() > 80_000 {
        let mut end = 80_000;
        while end > 0 && !conv_text.is_char_boundary(end) {
            end -= 1;
        }
        &conv_text[..end]
    } else {
        &conv_text
    };
    let summary = client.summarize(truncated, 2000).await?;

    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    eprintln!("\x1b[90m[context compressed → {}]\x1b[0m", filename);

    Ok(vec![
        Message::user(&format!(
            "[Context compressed. Transcript saved to {}]\n\n{}",
            path.display(),
            summary
        )),
        Message::assistant(
            Some("Understood. Continuing with the summarized context.".into()),
            None,
        ),
    ])
}

/// Run microcompact always; run auto_compact if over threshold. Returns messages.
pub async fn maybe_compact(
    client: &LlmClient,
    messages: &mut Vec<Message>,
    context_tokens: u32,
) -> Result<()> {
    let threshold = token_threshold(context_tokens);
    let est = estimate_tokens(messages);
    microcompact(messages, est > threshold * 8 / 10);
    if estimate_tokens(messages) > threshold {
        eprintln!("\x1b[90m[auto-compact triggered]\x1b[0m");
        let new_messages = auto_compact(client, messages).await?;
        *messages = new_messages;
    }
    Ok(())
}
