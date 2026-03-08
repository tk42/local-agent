"""
context.py - Context compression utilities

Prevents context overflow in long sessions via:
1. microcompact: Truncate old tool results to "[cleared]"
2. auto_compact: Summarize the entire conversation and restart with summary
3. estimate_tokens: Rough token count from JSON length
"""

import json
import time
from pathlib import Path

import llm_client

TOKEN_THRESHOLD = 80000  # Auto-compact when estimated tokens exceed this
TRANSCRIPT_DIR = Path.cwd() / ".transcripts"


def estimate_tokens(messages: list) -> int:
    """Rough estimate: ~4 chars per token."""
    return len(json.dumps(messages, default=str)) // 4


def microcompact(messages: list) -> None:
    """
    Clear old tool result content to save space.
    Keeps only the 3 most recent tool results intact.
    """
    tool_results = []
    for msg in messages:
        if msg.get("role") == "tool":
            tool_results.append(msg)

    if len(tool_results) <= 3:
        return

    for msg in tool_results[:-3]:
        content = msg.get("content", "")
        if isinstance(content, str) and len(content) > 200:
            msg["content"] = "[cleared]"


def auto_compact(messages: list) -> list:
    """
    Save transcript to disk, then summarize and return a fresh 2-message context.
    """
    TRANSCRIPT_DIR.mkdir(exist_ok=True)
    path = TRANSCRIPT_DIR / f"transcript_{int(time.time())}.jsonl"
    with open(path, "w") as f:
        for msg in messages:
            f.write(json.dumps(msg, default=str) + "\n")

    conv_text = json.dumps(messages, default=str)[:80000]
    summary = llm_client.summarize(conv_text)

    print(f"\033[90m[context compressed → {path.name}]\033[0m")

    return [
        {"role": "user", "content": f"[Context compressed. Transcript saved to {path}]\n\n{summary}"},
        {"role": "assistant", "content": "Understood. Continuing with the summarized context."},
    ]


def maybe_compact(messages: list) -> list:
    """Run microcompact always; run auto_compact if over threshold. Returns messages."""
    microcompact(messages)
    if estimate_tokens(messages) > TOKEN_THRESHOLD:
        print("\033[90m[auto-compact triggered]\033[0m")
        return auto_compact(messages)
    return messages
