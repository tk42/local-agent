///
/// todo_manager.rs - TodoWrite tool for structured task tracking
///
/// The LLM writes to this structured list to track progress on multi-step tasks.
/// Includes a nag reminder mechanism when the agent forgets to update todos.
///
use serde_json::Value;

// ---------------------------------------------------------------------------
// TodoManager
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct TodoItem {
    content: String,
    status: String,
}

#[derive(Debug)]
pub struct TodoManager {
    items: Vec<TodoItem>,
}

impl TodoManager {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Validate and replace the todo list. Returns rendered view.
    pub fn update(&mut self, items_value: &Value) -> Result<String, String> {
        let items = items_value
            .as_array()
            .ok_or("items must be an array")?;

        let mut validated = Vec::new();
        let mut in_progress_count = 0;

        for (i, item) in items.iter().enumerate() {
            let content = item["content"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            let status = item["status"]
                .as_str()
                .unwrap_or("pending")
                .to_lowercase();

            if content.is_empty() {
                return Err(format!("Item {}: content required", i));
            }
            if !["pending", "in_progress", "completed"].contains(&status.as_str()) {
                return Err(format!("Item {}: invalid status '{}'", i, status));
            }
            if status == "in_progress" {
                in_progress_count += 1;
            }
            validated.push(TodoItem { content, status });
        }

        if validated.len() > 20 {
            return Err("Max 20 todos".into());
        }
        if in_progress_count > 1 {
            return Err("Only one in_progress item allowed at a time".into());
        }

        self.items = validated;
        Ok(self.render())
    }

    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return "No todos.".into();
        }
        let mut lines: Vec<String> = self
            .items
            .iter()
            .map(|item| {
                let marker = match item.status.as_str() {
                    "completed" => "[x]",
                    "in_progress" => "[>]",
                    "pending" => "[ ]",
                    _ => "[?]",
                };
                format!("{} {}", marker, item.content)
            })
            .collect();

        let done = self.items.iter().filter(|t| t.status == "completed").count();
        lines.push(format!("\n({}/{} completed)", done, self.items.len()));
        lines.join("\n")
    }

    pub fn has_open_items(&self) -> bool {
        self.items.iter().any(|item| item.status != "completed")
    }
}

// ---------------------------------------------------------------------------
// Tool dispatch
// ---------------------------------------------------------------------------

pub fn handle_todo_write(manager: &mut TodoManager, args: &Value) -> String {
    match manager.update(&args["items"]) {
        Ok(rendered) => rendered,
        Err(e) => format!("Error: {}", e),
    }
}

// ---------------------------------------------------------------------------
// OpenAI-format tool definition
// ---------------------------------------------------------------------------

pub fn todo_tool_definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "todo_write",
            "description": "Create or update a structured task tracking list. Send the FULL list each time (not just changes). Only one item can be in_progress at a time.",
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
                                    "description": "Task status."
                                }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["items"]
            }
        }
    })
}
