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

pub const TOKEN_THRESHOLD: usize = 80_000;

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
/// Keeps only the 3 most recent tool results intact.
pub fn microcompact(messages: &mut [Message]) {
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "tool")
        .map(|(i, _)| i)
        .collect();

    if tool_indices.len() <= 3 {
        return;
    }

    let to_clear = &tool_indices[..tool_indices.len() - 3];
    for &idx in to_clear {
        if let Some(ref content) = messages[idx].content {
            if content.len() > 200 {
                messages[idx].content = Some("[cleared]".into());
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
        &conv_text[..80_000]
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
) -> Result<()> {
    microcompact(messages);
    if estimate_tokens(messages) > TOKEN_THRESHOLD {
        eprintln!("\x1b[90m[auto-compact triggered]\x1b[0m");
        let new_messages = auto_compact(client, messages).await?;
        *messages = new_messages;
    }
    Ok(())
}
