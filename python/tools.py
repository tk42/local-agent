"""
tools.py - Tool definitions and handlers

Defines OpenAI-format tool schemas and their execution handlers.
Tools: bash, read_file, write_file, edit_file, list_directory, grep_search
"""

import os
import subprocess
from pathlib import Path

WORKDIR = Path.cwd()


def safe_path(p: str) -> Path:
    """Resolve path relative to WORKDIR, block escapes."""
    path = (WORKDIR / p).resolve()
    if not path.is_relative_to(WORKDIR):
        raise ValueError(f"Path escapes workspace: {p}")
    return path


# ---------------------------------------------------------------------------
# Tool handlers
# ---------------------------------------------------------------------------

def run_bash(command: str) -> str:
    dangerous = ["rm -rf /", "rm -rf ~", "sudo rm", "shutdown", "reboot", "> /dev/", "mkfs"]
    if any(d in command for d in dangerous):
        return "Error: Dangerous command blocked"
    try:
        r = subprocess.run(
            command, shell=True, cwd=str(WORKDIR),
            capture_output=True, text=True, timeout=120,
        )
        out = (r.stdout + r.stderr).strip()
        return out[:50000] if out else "(no output)"
    except subprocess.TimeoutExpired:
        return "Error: Timeout (120s)"


def run_read_file(path: str, offset: int = 0, limit: int = 0) -> str:
    try:
        lines = safe_path(path).read_text().splitlines()
        total = len(lines)
        if offset > 0:
            lines = lines[offset - 1:]  # 1-indexed
        if limit > 0:
            lines = lines[:limit]
        numbered = [f"{i + (offset or 1):4d}  {line}" for i, line in enumerate(lines)]
        result = "\n".join(numbered)
        if len(result) > 50000:
            result = result[:50000] + "\n... (truncated)"
        suffix = f"\n[{total} lines total]" if total > len(lines) else ""
        return result + suffix
    except Exception as e:
        return f"Error: {e}"


def run_write_file(path: str, content: str) -> str:
    try:
        fp = safe_path(path)
        fp.parent.mkdir(parents=True, exist_ok=True)
        fp.write_text(content)
        return f"Wrote {len(content)} bytes to {path}"
    except Exception as e:
        return f"Error: {e}"


def run_edit_file(path: str, old_text: str, new_text: str) -> str:
    try:
        fp = safe_path(path)
        c = fp.read_text()
        if old_text not in c:
            return f"Error: Text not found in {path}"
        count = c.count(old_text)
        c = c.replace(old_text, new_text, 1)
        fp.write_text(c)
        info = f" (1 of {count} occurrences replaced)" if count > 1 else ""
        return f"Edited {path}{info}"
    except Exception as e:
        return f"Error: {e}"


def run_list_directory(path: str = ".", max_depth: int = 2) -> str:
    try:
        target = safe_path(path)
        if not target.is_dir():
            return f"Error: {path} is not a directory"
        lines = []
        _walk(target, target, lines, max_depth, depth=0)
        result = "\n".join(lines)
        return result[:50000] if result else "(empty directory)"
    except Exception as e:
        return f"Error: {e}"


def _walk(base: Path, current: Path, lines: list, max_depth: int, depth: int):
    if depth > max_depth:
        return
    try:
        entries = sorted(current.iterdir(), key=lambda e: (not e.is_dir(), e.name.lower()))
    except PermissionError:
        return
    for entry in entries:
        if entry.name.startswith("."):
            continue
        rel = entry.relative_to(base)
        indent = "  " * depth
        if entry.is_dir():
            lines.append(f"{indent}{rel}/")
            _walk(base, entry, lines, max_depth, depth + 1)
        else:
            size = entry.stat().st_size
            lines.append(f"{indent}{rel}  ({size} bytes)")


def run_grep_search(pattern: str, path: str = ".", include: str = "") -> str:
    cmd = ["rg", "--no-heading", "--line-number", "--color=never", "-m", "50"]
    if include:
        cmd += ["-g", include]
    cmd += [pattern, str(safe_path(path))]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30, cwd=str(WORKDIR))
        out = (r.stdout + r.stderr).strip()
        return out[:50000] if out else "(no matches)"
    except FileNotFoundError:
        # rg not installed, fall back to grep
        cmd_fallback = ["grep", "-rn", "--color=never"]
        if include:
            cmd_fallback += ["--include", include]
        cmd_fallback += [pattern, str(safe_path(path))]
        try:
            r = subprocess.run(cmd_fallback, capture_output=True, text=True, timeout=30, cwd=str(WORKDIR))
            out = (r.stdout + r.stderr).strip()
            lines = out.splitlines()[:50]
            return "\n".join(lines) if lines else "(no matches)"
        except Exception as e:
            return f"Error: {e}"
    except subprocess.TimeoutExpired:
        return "Error: Search timeout (30s)"


# ---------------------------------------------------------------------------
# Tool dispatch map
# ---------------------------------------------------------------------------

TOOL_HANDLERS = {
    "bash":           lambda **kw: run_bash(kw["command"]),
    "read_file":      lambda **kw: run_read_file(kw["path"], kw.get("offset", 0), kw.get("limit", 0)),
    "write_file":     lambda **kw: run_write_file(kw["path"], kw["content"]),
    "edit_file":      lambda **kw: run_edit_file(kw["path"], kw["old_text"], kw["new_text"]),
    "list_directory": lambda **kw: run_list_directory(kw.get("path", "."), kw.get("max_depth", 2)),
    "grep_search":    lambda **kw: run_grep_search(kw["pattern"], kw.get("path", "."), kw.get("include", "")),
}


# ---------------------------------------------------------------------------
# OpenAI-format tool definitions
# ---------------------------------------------------------------------------

TOOL_DEFINITIONS = [
    {
        "type": "function",
        "function": {
            "name": "bash",
            "description": "Run a shell command and return stdout+stderr.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to execute."},
                },
                "required": ["command"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "read_file",
            "description": "Read file contents with line numbers. Use offset/limit for large files.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path":   {"type": "string", "description": "File path (relative to workspace root)."},
                    "offset": {"type": "integer", "description": "1-indexed start line (optional)."},
                    "limit":  {"type": "integer", "description": "Max lines to read (optional)."},
                },
                "required": ["path"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "write_file",
            "description": "Create or overwrite a file with the given content.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path":    {"type": "string", "description": "File path (relative to workspace root)."},
                    "content": {"type": "string", "description": "Full file content to write."},
                },
                "required": ["path", "content"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "edit_file",
            "description": "Replace the first occurrence of old_text with new_text in a file.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path":     {"type": "string", "description": "File path."},
                    "old_text": {"type": "string", "description": "Exact text to find (must match)."},
                    "new_text": {"type": "string", "description": "Replacement text."},
                },
                "required": ["path", "old_text", "new_text"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "list_directory",
            "description": "List files and directories in a tree structure.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path":      {"type": "string", "description": "Directory path (default: workspace root)."},
                    "max_depth": {"type": "integer", "description": "Max depth to traverse (default: 2)."},
                },
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "grep_search",
            "description": "Search for a regex pattern in files using ripgrep. Returns matching lines with file paths and line numbers.",
            "parameters": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex pattern to search for."},
                    "path":    {"type": "string", "description": "Directory or file to search in (default: workspace root)."},
                    "include": {"type": "string", "description": "Glob filter, e.g. '*.py' (optional)."},
                },
                "required": ["pattern"],
            },
        },
    },
]
