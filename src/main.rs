///
/// main.rs - Local Claude Code-like coding agent
///
/// A CLI coding agent powered by a local llama-server (OpenAI-compatible API).
/// Inspired by shareAI-lab/learn-claude-code.
///
/// Usage:
///     local-agent                    # Interactive REPL
///     local-agent "fix the bug"      # One-shot mode
///
/// REPL commands:
///     /compact   - Force context compression
///     /todos     - Show current todo list
///     /tokens    - Show estimated token usage
///     /help      - Show available commands
///     q / exit   - Quit
///
mod context;
mod llm_client;
mod todo_manager;
mod tools;

use std::path::PathBuf;

use anyhow::Result;
use serde_json::Value;

use llm_client::{LlmClient, LlmConfig, Message, MessageToolCall, FunctionCall};
use todo_manager::{TodoManager, handle_todo_write, todo_tool_definition};

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

fn system_prompt(workdir: &str) -> String {
    format!(
        r#"You are an expert coding agent working in: {}

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
6. Be concise in your responses. Show results, not explanations."#,
        workdir
    )
}

// ---------------------------------------------------------------------------
// Agent loop
// ---------------------------------------------------------------------------

async fn agent_loop(
    client: &LlmClient,
    messages: &mut Vec<Message>,
    all_tools: &[Value],
    todo: &mut TodoManager,
    workdir: &PathBuf,
) -> Result<()> {
    let mut rounds_without_todo: u32 = 0;
    let sys_prompt = system_prompt(&workdir.display().to_string());

    loop {
        // Context management
        context::maybe_compact(client, messages).await?;

        // Build messages with system prompt prepended
        let mut full_messages = vec![Message::system(&sys_prompt)];
        full_messages.extend(messages.iter().cloned());

        // LLM call
        let result = client.chat(&full_messages, Some(all_tools)).await?;

        // Build assistant message
        let msg_tool_calls = result.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .map(|tc| MessageToolCall {
                    id: tc.id.clone(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: tc.name.clone(),
                        arguments: tc.arguments_raw.clone(),
                    },
                })
                .collect()
        });

        messages.push(Message::assistant(result.content.clone(), msg_tool_calls));

        // No tool calls → done
        let tool_calls = match result.tool_calls {
            Some(ref tcs) if !tcs.is_empty() => tcs,
            _ => return Ok(()),
        };

        // Execute tool calls
        let mut used_todo = false;
        for tc in tool_calls {
            let name = &tc.name;
            let args = &tc.arguments;
            let tool_call_id = &tc.id;

            // Print tool invocation
            if name == "bash" {
                let cmd = args["command"].as_str().unwrap_or("");
                println!("\x1b[33m$ {}\x1b[0m", cmd);
            } else if name == "todo_write" {
                println!("\x1b[90m[updating todos]\x1b[0m");
            } else {
                let arg_summary = format_args_summary(args);
                println!("\x1b[90m> {}({})\x1b[0m", name, arg_summary);
            }

            // Execute
            let output = if name == "todo_write" {
                used_todo = true;
                handle_todo_write(todo, args)
            } else {
                tools::dispatch_tool(workdir, name, args)
            };

            // Print output (truncated)
            if name == "todo_write" {
                println!("\x1b[90m{}\x1b[0m", output);
            } else {
                let preview = if output.len() > 500 {
                    format!("{}\n... ({} chars total)", &output[..500], output.len())
                } else {
                    output.clone()
                };
                println!("{}", preview);
            }

            // Append tool result
            messages.push(Message::tool(tool_call_id, &output));
        }

        // Nag reminder for forgotten todos
        rounds_without_todo = if used_todo { 0 } else { rounds_without_todo + 1 };
        if todo.has_open_items() && rounds_without_todo >= 3 {
            messages.push(Message::user(
                "<reminder>You have open todo items. Please update them with todo_write.</reminder>",
            ));
        }
    }
}

fn format_args_summary(args: &Value) -> String {
    match args.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, v)| {
                let repr = format!("{}", v);
                let truncated = if repr.len() > 60 {
                    format!("{}...", &repr[..60])
                } else {
                    repr
                };
                format!("{}={}", k, truncated)
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// REPL
// ---------------------------------------------------------------------------

fn print_help() {
    println!(
        r#"
{bold}Local Agent - Commands{reset}
  /compact   Force context compression
  /todos     Show current todo list
  /tokens    Show estimated token usage
  /clear     Clear conversation history
  /help      Show this help
  q / exit   Quit
"#,
        bold = "\x1b[1m",
        reset = "\x1b[0m"
    );
}

async fn repl(client: &LlmClient, workdir: &PathBuf) -> Result<()> {
    let mut history: Vec<Message> = Vec::new();
    let mut todo = TodoManager::new();

    let mut all_tools = tools::tool_definitions();
    all_tools.push(todo_tool_definition());

    println!(
        "\x1b[1;36mLocal Agent\x1b[0m @ {}",
        workdir.display()
    );
    println!(
        "Model: {} | Server: {}",
        client.config.model, client.config.base_url
    );
    println!("Type /help for commands, q to quit.\n");

    let mut rl = rustyline::DefaultEditor::new()?;

    loop {
        let readline = rl.readline("\x1b[36m>>> \x1b[0m");
        match readline {
            Ok(line) => {
                let stripped = line.trim().to_lowercase();
                if stripped == "q" || stripped == "exit" || stripped == "quit" {
                    println!("Bye!");
                    break;
                }
                if stripped.is_empty() {
                    continue;
                }
                rl.add_history_entry(&line).ok();

                if stripped == "/help" {
                    print_help();
                    continue;
                }
                if stripped == "/compact" {
                    if history.is_empty() {
                        println!("Nothing to compact.");
                    } else {
                        println!("\x1b[90m[manual compact]\x1b[0m");
                        let new_msgs = context::auto_compact(client, &history).await?;
                        history = new_msgs;
                    }
                    continue;
                }
                if stripped == "/todos" {
                    println!("{}", todo.render());
                    continue;
                }
                if stripped == "/tokens" {
                    let tokens = context::estimate_tokens(&history);
                    println!(
                        "Estimated tokens: ~{} (threshold: {})",
                        tokens,
                        context::TOKEN_THRESHOLD
                    );
                    continue;
                }
                if stripped == "/clear" {
                    history.clear();
                    println!("Conversation cleared.");
                    continue;
                }

                history.push(Message::user(line.trim()));

                match agent_loop(client, &mut history, &all_tools, &mut todo, workdir).await {
                    Ok(()) => {}
                    Err(e) => {
                        eprintln!("\x1b[31m[error] {}\x1b[0m", e);
                    }
                }
                println!();
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("\nBye!");
                break;
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                println!("\nBye!");
                break;
            }
            Err(e) => {
                eprintln!("Error: {}", e);
                break;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// One-shot mode
// ---------------------------------------------------------------------------

async fn one_shot(client: &LlmClient, query: &str, workdir: &PathBuf) -> Result<()> {
    let mut history = vec![Message::user(query)];
    let mut todo = TodoManager::new();
    let mut all_tools = tools::tool_definitions();
    all_tools.push(todo_tool_definition());

    agent_loop(client, &mut history, &all_tools, &mut todo, workdir).await
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let config = LlmConfig::from_env();
    let client = LlmClient::new(config);
    let workdir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        repl(&client, &workdir).await
    } else {
        let query = args.join(" ");
        one_shot(&client, &query, &workdir).await
    }
}
