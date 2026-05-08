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
mod skills;
mod todo_manager;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, EventHandler, KeyCode, KeyEvent, Modifiers, RepeatCount};
use serde_json::Value;

use llm_client::{LlmClient, LlmConfig, Message, MessageToolCall, FunctionCall};
use skills::{SkillRegistry, handle_load_skill};
use todo_manager::{TodoManager, handle_todo_write, todo_tool_definition};

const PLAN_BLOCKED_TOOLS: &[&str] = &["bash", "write_file", "edit_file"];

struct PlanModeToggler {
    flag: Arc<AtomicBool>,
}

impl ConditionalEventHandler for PlanModeToggler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext,
    ) -> Option<Cmd> {
        let prev = self.flag.fetch_xor(true, Ordering::SeqCst);
        let now_on = !prev;
        eprint!(
            "\r\x1b[2K\x1b[35m[plan mode: {}]\x1b[0m\n",
            if now_on { "ON" } else { "OFF" }
        );
        Some(Cmd::Noop)
    }
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

fn system_prompt(workdir: &str, skills: &SkillRegistry, plan_mode: bool) -> String {
    let mut out = format!(
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
    );
    if let Some(section) = skills.system_prompt_section() {
        out.push_str(&section);
    }
    if plan_mode {
        out.push_str(
            "\n\n## Mode: PLAN\nYou are in PLAN mode. Investigate the codebase first, then propose a numbered, Markdown-formatted plan of the work you would do.\n- Allowed: read_file, list_directory, grep_search, load_skill, todo_write\n- Forbidden: bash, write_file, edit_file (these tools will be blocked and return errors)\nDo not modify anything. End your response with: \"Switch off plan mode (Shift+Tab or /plan) to execute.\"",
        );
    }
    out
}

// ---------------------------------------------------------------------------
// Agent loop
// ---------------------------------------------------------------------------

async fn agent_loop(
    client: &LlmClient,
    messages: &mut Vec<Message>,
    all_tools: &[Value],
    todo: &mut TodoManager,
    skills: &SkillRegistry,
    plan_mode: &Arc<AtomicBool>,
    workdir: &PathBuf,
) -> Result<()> {
    let mut rounds_without_todo: u32 = 0;

    loop {
        // Re-evaluate plan mode each iteration so user can toggle mid-loop.
        let in_plan = plan_mode.load(Ordering::Relaxed);
        let sys_prompt = system_prompt(&workdir.display().to_string(), skills, in_plan);

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

        let content_for_msg = match (&result.content, &msg_tool_calls) {
            (None, None) => Some(String::new()),
            _ => result.content.clone(),
        };
        messages.push(Message::assistant(content_for_msg, msg_tool_calls));

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
            } else if name == "load_skill" {
                let skill_name = args["name"].as_str().unwrap_or("");
                println!("\x1b[35m[loading skill: {}]\x1b[0m", skill_name);
            } else {
                let arg_summary = format_args_summary(args);
                println!("\x1b[90m> {}({})\x1b[0m", name, arg_summary);
            }

            // Execute (gate destructive tools while plan mode is on)
            let output = if in_plan && PLAN_BLOCKED_TOOLS.contains(&name.as_str()) {
                println!("\x1b[31m[blocked: plan mode]\x1b[0m");
                format!(
                    "Error: tool '{}' is blocked while plan mode is active. Toggle off with Shift+Tab or /plan to execute.",
                    name
                )
            } else if name == "todo_write" {
                used_todo = true;
                handle_todo_write(todo, args)
            } else if name == "load_skill" {
                handle_load_skill(skills, args)
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
  /skills    List loaded skills
  /plan      Toggle plan mode (alias for Shift+Tab)
  /clear     Clear conversation history
  /help      Show this help
  q / exit   Quit

  Shift+Tab  Toggle plan mode (read-only investigation; bash/write/edit blocked)
"#,
        bold = "\x1b[1m",
        reset = "\x1b[0m"
    );
}

fn make_prompt(plan_mode: &Arc<AtomicBool>) -> String {
    if plan_mode.load(Ordering::Relaxed) {
        "\x1b[35m[PLAN]\x1b[0m \x1b[36m>>> \x1b[0m".to_string()
    } else {
        "\x1b[36m>>> \x1b[0m".to_string()
    }
}

async fn repl(client: &LlmClient, skills: &SkillRegistry, workdir: &PathBuf) -> Result<()> {
    let mut history: Vec<Message> = Vec::new();
    let mut todo = TodoManager::new();
    let plan_mode = Arc::new(AtomicBool::new(false));

    let mut all_tools = tools::tool_definitions();
    all_tools.push(todo_tool_definition());
    if let Some(def) = skills.tool_definition() {
        all_tools.push(def);
    }

    println!(
        "\x1b[1;36mLocal Agent\x1b[0m @ {}",
        workdir.display()
    );
    println!(
        "Model: {} | Server: {}",
        client.config.model, client.config.base_url
    );
    if !skills.is_empty() {
        println!("Skills: {} loaded (use /skills to list)", skills.list().len());
    }
    println!("Type /help for commands, q to quit. Shift+Tab toggles plan mode.\n");

    let mut rl = rustyline::DefaultEditor::new()?;
    rl.bind_sequence(
        Event::KeySeq(vec![KeyEvent(KeyCode::BackTab, Modifiers::NONE)]),
        EventHandler::Conditional(Box::new(PlanModeToggler {
            flag: plan_mode.clone(),
        })),
    );

    loop {
        let prompt = make_prompt(&plan_mode);
        let readline = rl.readline(&prompt);
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
                if stripped == "/skills" {
                    if skills.is_empty() {
                        println!("No skills loaded. Place SKILL.md files under ./skills/<name>/ next to the binary.");
                    } else {
                        for s in skills.list() {
                            println!("  \x1b[35m{}\x1b[0m: {}", s.name, s.description);
                        }
                    }
                    continue;
                }
                if stripped == "/plan" {
                    let prev = plan_mode.fetch_xor(true, Ordering::SeqCst);
                    let now_on = !prev;
                    println!(
                        "\x1b[35m[plan mode: {}]\x1b[0m",
                        if now_on { "ON" } else { "OFF" }
                    );
                    continue;
                }

                history.push(Message::user(line.trim()));

                match agent_loop(client, &mut history, &all_tools, &mut todo, skills, &plan_mode, workdir).await {
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

async fn one_shot(
    client: &LlmClient,
    skills: &SkillRegistry,
    query: &str,
    workdir: &PathBuf,
) -> Result<()> {
    let mut history = vec![Message::user(query)];
    let mut todo = TodoManager::new();
    let mut all_tools = tools::tool_definitions();
    all_tools.push(todo_tool_definition());
    if let Some(def) = skills.tool_definition() {
        all_tools.push(def);
    }
    let plan_mode = Arc::new(AtomicBool::new(false));

    agent_loop(client, &mut history, &all_tools, &mut todo, skills, &plan_mode, workdir).await
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
    let skills = SkillRegistry::load();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        repl(&client, &skills, &workdir).await
    } else {
        let query = args.join(" ");
        one_shot(&client, &skills, &query, &workdir).await
    }
}
