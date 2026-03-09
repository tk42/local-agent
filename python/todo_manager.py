"""
todo_manager.py - TodoWrite tool for structured task tracking

The LLM writes to this structured list to track progress on multi-step tasks.
Includes a nag reminder mechanism when the agent forgets to update todos.
"""


class TodoManager:
    def __init__(self):
        self.items: list[dict] = []

    def update(self, items: list) -> str:
        """Validate and replace the todo list. Returns rendered view."""
        validated = []
        in_progress_count = 0
        for i, item in enumerate(items):
            content = str(item.get("content", "")).strip()
            status = str(item.get("status", "pending")).lower()
            if not content:
                raise ValueError(f"Item {i}: content required")
            if status not in ("pending", "in_progress", "completed"):
                raise ValueError(f"Item {i}: invalid status '{status}'")
            if status == "in_progress":
                in_progress_count += 1
            validated.append({"content": content, "status": status})
        if len(validated) > 20:
            raise ValueError("Max 20 todos")
        if in_progress_count > 1:
            raise ValueError("Only one in_progress item allowed at a time")
        self.items = validated
        return self.render()

    def render(self) -> str:
        if not self.items:
            return "No todos."
        lines = []
        for item in self.items:
            marker = {"completed": "[x]", "in_progress": "[>]", "pending": "[ ]"}.get(item["status"], "[?]")
            lines.append(f"{marker} {item['content']}")
        done = sum(1 for t in self.items if t["status"] == "completed")
        lines.append(f"\n({done}/{len(self.items)} completed)")
        return "\n".join(lines)

    def has_open_items(self) -> bool:
        return any(item.get("status") != "completed" for item in self.items)


# OpenAI-format tool definition
TODO_TOOL_DEFINITION = {
    "type": "function",
    "function": {
        "name": "todo_write",
        "description": (
            "Create or update a structured task tracking list. "
            "Send the FULL list each time (not just changes). "
            "Only one item can be in_progress at a time."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "description": "Complete list of todo items.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Task description."},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "Task status.",
                            },
                        },
                        "required": ["content", "status"],
                    },
                },
            },
            "required": ["items"],
        },
    },
}
