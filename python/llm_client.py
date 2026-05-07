"""
llm_client.py - OpenAI SDK wrapper for llama-server

Connects to a local llama-server (or any OpenAI-compatible endpoint)
via the OpenAI Python SDK. Handles tool-calling response parsing.
"""

from __future__ import annotations

import json
import os
import sys
import time
from typing import Optional

from openai import OpenAI
from dotenv import load_dotenv

load_dotenv(override=True)

BASE_URL = os.getenv("LLM_BASE_URL", "http://localhost:8080/v1")
API_KEY = os.getenv("LLM_API_KEY", "sk-no-key-required")
MODEL = os.getenv("LLM_MODEL", "any-model-name")
MAX_TOKENS = int(os.getenv("LLM_MAX_TOKENS", "32768"))
TEMPERATURE = float(os.getenv("LLM_TEMPERATURE", "0.6"))

client = OpenAI(base_url=BASE_URL, api_key=API_KEY)


def chat(messages: list, tools: list | None = None, stream: bool = True) -> dict:
    """
    Send a chat completion request. Returns a dict with:
      - role: "assistant"
      - content: text content (str or None)
      - tool_calls: list of tool call dicts (or None)
      - finish_reason: str
    """
    kwargs = dict(
        model=MODEL,
        messages=messages,
        max_tokens=MAX_TOKENS,
        temperature=TEMPERATURE,
    )
    if tools:
        kwargs["tools"] = tools
        kwargs["tool_choice"] = "auto"

    if stream:
        return _stream_chat(kwargs)
    else:
        return _sync_chat(kwargs)


def _sync_chat(kwargs: dict) -> dict:
    for attempt in range(3):
        try:
            response = client.chat.completions.create(**kwargs)
            choice = response.choices[0]
            return _parse_choice(choice)
        except Exception as e:
            if _is_connection_error(e):
                _die_connection_error()
            if attempt < 2:
                print(f"\033[31m[LLM retry {attempt+1}] {e}\033[0m", file=sys.stderr)
                time.sleep(2)
            else:
                raise


def _stream_chat(kwargs: dict) -> dict:
    kwargs["stream"] = True
    for attempt in range(3):
        try:
            stream = client.chat.completions.create(**kwargs)
            return _consume_stream(stream)
        except Exception as e:
            if _is_connection_error(e):
                _die_connection_error()
            if attempt < 2:
                print(f"\033[31m[LLM retry {attempt+1}] {e}\033[0m", file=sys.stderr)
                time.sleep(2)
            else:
                raise


def _is_connection_error(e: Exception) -> bool:
    return "Connection" in type(e).__name__ or "ConnectError" in str(type(e))


def _die_connection_error():
    print(
        f"\n\033[31;1m[Error] llama-server に接続できません: {BASE_URL}\033[0m\n"
        f"llama-server が起動しているか確認してください:\n"
        f"  ./apps/scripts/start-llama-server.sh\n",
        file=sys.stderr,
    )
    sys.exit(1)


def _consume_stream(stream) -> dict:
    """Consume an SSE stream, printing text chunks in real-time."""
    content_parts = []
    tool_calls_acc = {}  # index -> {id, function: {name, arguments}}
    finish_reason = None

    for chunk in stream:
        if not chunk.choices:
            continue
        delta = chunk.choices[0].delta
        fr = chunk.choices[0].finish_reason
        if fr:
            finish_reason = fr

        # Text content
        if delta.content:
            print(delta.content, end="", flush=True)
            content_parts.append(delta.content)

        # Tool calls (streamed incrementally)
        if delta.tool_calls:
            for tc in delta.tool_calls:
                idx = tc.index
                if idx not in tool_calls_acc:
                    tool_calls_acc[idx] = {
                        "id": tc.id or "",
                        "function": {"name": tc.function.name or "", "arguments": ""},
                    }
                else:
                    if tc.id:
                        tool_calls_acc[idx]["id"] = tc.id
                    if tc.function and tc.function.name:
                        tool_calls_acc[idx]["function"]["name"] = tc.function.name
                if tc.function and tc.function.arguments:
                    tool_calls_acc[idx]["function"]["arguments"] += tc.function.arguments

    # Newline after streamed text
    if content_parts:
        print()

    content = "".join(content_parts) if content_parts else None
    tool_calls = None
    if tool_calls_acc:
        tool_calls = []
        for idx in sorted(tool_calls_acc.keys()):
            tc = tool_calls_acc[idx]
            # Parse arguments JSON
            try:
                args = json.loads(tc["function"]["arguments"]) if tc["function"]["arguments"] else {}
            except json.JSONDecodeError:
                args = {"_raw": tc["function"]["arguments"]}
            tool_calls.append({
                "id": tc["id"],
                "name": tc["function"]["name"],
                "arguments": args,
                "arguments_raw": tc["function"]["arguments"],
            })

    return {
        "role": "assistant",
        "content": content,
        "tool_calls": tool_calls,
        "finish_reason": finish_reason or "stop",
    }


def _parse_choice(choice) -> dict:
    """Parse a non-streamed choice object."""
    msg = choice.message
    content = msg.content
    tool_calls = None

    if msg.tool_calls:
        tool_calls = []
        for tc in msg.tool_calls:
            try:
                args = json.loads(tc.function.arguments) if tc.function.arguments else {}
            except json.JSONDecodeError:
                args = {"_raw": tc.function.arguments}
            tool_calls.append({
                "id": tc.id,
                "name": tc.function.name,
                "arguments": args,
                "arguments_raw": tc.function.arguments,
            })

    return {
        "role": "assistant",
        "content": content,
        "tool_calls": tool_calls,
        "finish_reason": choice.finish_reason or "stop",
    }


def summarize(text: str, max_tokens: int = 2000) -> str:
    """Use the LLM to produce a summary (for context compression)."""
    resp = client.chat.completions.create(
        model=MODEL,
        messages=[{"role": "user", "content": f"Summarize the following conversation for continuity:\n\n{text}"}],
        max_tokens=max_tokens,
        temperature=0.3,
    )
    return resp.choices[0].message.content or "(empty summary)"
