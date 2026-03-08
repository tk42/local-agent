#!/usr/bin/env python3
"""
agent.py - Local Claude Code-like coding agent

A CLI coding agent powered by a local llama-server (OpenAI-compatible API).
Inspired by shareAI-lab/learn-claude-code.

Usage:
    python agent.py                    # Interactive REPL
    python agent.py "fix the bug"      # One-shot mode

REPL commands:
    /compact   - Force context compression
    /todos     - Show current todo list
    /tokens    - Show estimated token usage
    /help      - Show available commands
    q / exit   - Quit
"""

import json
import os
import sys
from pathlib import Path

# Ensure the package directory is on sys.path for sibling imports
sys.path.insert(0, str(Path(__file__).parent))

import llm_client
import context
from tools import TOOL_DEFINITIONS, TOOL_HANDLERS, WORKDIR
from todo_manager import TodoManager, TODO_TOOL_DEFINITION

# ---------------------------------------------------------------------------
# System prompt
# ---------------------------------------------------------------------------

SYSTEM_PROMPT = f"""You are an expert coding agent working in: {WORKDIR}

Your capabilities:
- Execute shell commands (bash)
- Read, write, and edit files
- Search code with grep
- List directory contents
- Track tasks with todo_write

Rules:
1. Use tools to accomplish tasks. Don't just explain — act.
2. Read files before editing to understand context.
3. For multi-step tasks, use todo_write to track progress.
4. Prefer small, targeted edits over full file rewrites.
5. Verify your changes work (e.g., run tests, check syntax).
6. Be concise in your responses. Show results, not explanations.
"""

# ---------------------------------------------------------------------------
# Globals
# ---------------------------------------------------------------------------

TODO = TodoManager()
ALL_TOOLS = TOOL_DEFINITIONS + [TODO_TOOL_DEFINITION]

# Extend handlers with todo
TOOL_HANDLERS["todo_write"] = lambda **kw: TODO.update(kw["items"])

# ---------------------------------------------------------------------------
# Agent loop
# ---------------------------------------------------------------------------

def agent_loop(messages: list) -> None:
    """
    The core agent loop:
      1. Compress context if needed
      2. Call LLM with tools
      3. If tool_calls → execute → append results → loop
      4. If no tool_calls → done
    """
    rounds_without_todo = 0

    while True:
        # Context management
        messages[:] = context.maybe_compact(messages)

        # LLM call
        result = llm_client.chat(
            messages=[{"role": "system", "content": SYSTEM_PROMPT}] + messages,
            tools=ALL_TOOLS,
            stream=True,
        )

        # Append assistant message
        assistant_msg = {"role": "assistant"}
        if result["content"]:
            assistant_msg["content"] = result["content"]
        if result["tool_calls"]:
            assistant_msg["tool_calls"] = [
                {
                    "id": tc["id"],
                    "type": "function",
                    "function": {
                        "name": tc["name"],
                        "arguments": tc["arguments_raw"],
                    },
                }
                for tc in result["tool_calls"]
            ]
        # Ensure at least content key exists
        if "content" not in assistant_msg:
            assistant_msg["content"] = None
        messages.append(assistant_msg)

        # No tool calls → done
        if not result["tool_calls"]:
            return

        # Execute tool calls
        used_todo = False
        for tc in result["tool_calls"]:
            name = tc["name"]
            args = tc["arguments"]
            tool_call_id = tc["id"]

            # Print tool invocation
            if name == "bash":
                print(f"\033[33m$ {args.get('command', '')}\033[0m")
            elif name == "todo_write":
                print(f"\033[90m[updating todos]\033[0m")
            else:
                arg_summary = ", ".join(f"{k}={repr(v)[:60]}" for k, v in args.items())
                print(f"\033[90m> {name}({arg_summary})\033[0m")

            # Execute
            handler = TOOL_HANDLERS.get(name)
            if handler:
                try:
                    output = handler(**args)
                except Exception as e:
                    output = f"Error: {e}"
            else:
                output = f"Error: Unknown tool '{name}'"

            # Print output (truncated)
            output_str = str(output)
            if name == "todo_write":
                print(f"\033[90m{output_str}\033[0m")
            else:
                preview = output_str[:500]
                if len(output_str) > 500:
                    preview += f"\n... ({len(output_str)} chars total)"
                print(preview)

            if name == "todo_write":
                used_todo = True

            # Append tool result
            messages.append({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": output_str,
            })

        # Nag reminder for forgotten todos
        rounds_without_todo = 0 if used_todo else rounds_without_todo + 1
        if TODO.has_open_items() and rounds_without_todo >= 3:
            messages.append({
                "role": "user",
                "content": "<reminder>You have open todo items. Please update them with todo_write.</reminder>",
            })


# ---------------------------------------------------------------------------
# REPL
# ---------------------------------------------------------------------------

def print_help():
    print("""
\033[1mLocal Agent - Commands\033[0m
  /compact   Force context compression
  /todos     Show current todo list
  /tokens    Show estimated token usage
  /clear     Clear conversation history
  /help      Show this help
  q / exit   Quit
""")


def repl():
    history: list[dict] = []
    print(f"\033[1;36mLocal Agent\033[0m @ {WORKDIR}")
    print(f"Model: {llm_client.MODEL} | Server: {llm_client.BASE_URL}")
    print("Type /help for commands, q to quit.\n")

    while True:
        try:
            query = input("\033[36m>>> \033[0m")
        except (EOFError, KeyboardInterrupt):
            print("\nBye!")
            break

        stripped = query.strip().lower()
        if stripped in ("q", "exit", "quit"):
            print("Bye!")
            break
        if stripped == "":
            continue
        if stripped == "/help":
            print_help()
            continue
        if stripped == "/compact":
            if history:
                print("\033[90m[manual compact]\033[0m")
                history[:] = context.auto_compact(history)
            else:
                print("Nothing to compact.")
            continue
        if stripped == "/todos":
            print(TODO.render())
            continue
        if stripped == "/tokens":
            tokens = context.estimate_tokens(history)
            print(f"Estimated tokens: ~{tokens:,} (threshold: {context.TOKEN_THRESHOLD:,})")
            continue
        if stripped == "/clear":
            history.clear()
            print("Conversation cleared.")
            continue

        history.append({"role": "user", "content": query})

        try:
            agent_loop(history)
        except KeyboardInterrupt:
            print("\n\033[33m[interrupted]\033[0m")
            # Add a note so LLM knows it was interrupted
            history.append({"role": "assistant", "content": "(interrupted by user)"})
        except Exception as e:
            print(f"\033[31m[error] {e}\033[0m", file=sys.stderr)

        print()


def one_shot(query: str):
    """Run a single query and exit."""
    history = [{"role": "user", "content": query}]
    try:
        agent_loop(history)
    except KeyboardInterrupt:
        print("\n\033[33m[interrupted]\033[0m")


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    if len(sys.argv) > 1:
        one_shot(" ".join(sys.argv[1:]))
    else:
        repl()
