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
mod tool_call_stream;
mod tools;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, EventHandler, KeyCode, KeyEvent, Modifiers, RepeatCount};
use serde_json::Value;

use llm_client::{LlmClient, LlmConfig, Message, MessageToolCall, FunctionCallSerde};
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
    let os = std::env::consts::OS;
    let shell_hint = if cfg!(windows) {
        "Windows cmd.exe (NOT bash). Use Windows-style paths (C:\\foo\\bar) and cmd syntax. \
         Do NOT use bash-isms like '&&' chaining beyond cmd's support, '2>/dev/null', backticks, or POSIX globs. \
         For redirection use '> NUL 2>&1'. For pipelines use '|'. The 'bash' tool actually invokes cmd /C on this OS."
    } else {
        "POSIX sh. Use forward-slash paths and standard sh syntax."
    };
    let mut out = format!(
        r#"You are an expert coding agent working in: {workdir}
Operating system: {os}
Shell environment: {shell_hint}

Your capabilities:
- Execute shell commands (bash tool — note: maps to cmd.exe on Windows)
- Read, write, and edit files
- Search code with grep_search (uses ripgrep, falls back to PowerShell Select-String on Windows)
- List directory contents
- Track tasks with todo_write

Rules:
1. Use tools to accomplish tasks. Don't just explain — act.
2. Read files before editing to understand context.
3. For multi-step tasks, use todo_write to track progress.
4. Prefer small, targeted edits over full file rewrites.
5. Verify your changes work (e.g., run tests, check syntax).
6. Be concise in your responses. Show results, not explanations.
7. Match shell syntax to the OS above. Do not invent paths from other operating systems."#,
        workdir = workdir,
        os = os,
        shell_hint = shell_hint
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

        // Surface non-stop finish reasons so the user knows why a turn ended early.
        match result.finish_reason.as_str() {
            "stop" | "tool_calls" | "" => {}
            "length" => eprintln!(
                "\x1b[33m[warn] response truncated by max_tokens limit (LLM_MAX_TOKENS={})\x1b[0m",
                client.config.max_tokens
            ),
            "content_filter" => eprintln!(
                "\x1b[33m[warn] response stopped by content filter\x1b[0m"
            ),
            other => eprintln!(
                "\x1b[33m[warn] unexpected finish_reason: {}\x1b[0m",
                other
            ),
        }

        // Build assistant message. Always send `arguments` as a valid JSON string
        // (re-serialize the parsed Value), and always send `content` as a string
        // — `null` content with tool_calls breaks some llama-server jinja templates.
        let msg_tool_calls = result.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .map(|tc| MessageToolCall {
                    id: tc.id.clone(),
                    call_type: "function".into(),
                    function: FunctionCallSerde {
                        name: tc.name.clone(),
                        arguments: serde_json::to_string(&tc.arguments)
                            .unwrap_or_else(|_| "{}".into()),
                    },
                })
                .collect()
        });

        let content_for_msg = Some(result.content.clone().unwrap_or_default());
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

            // Execute (gate destructive tools while plan mode is on).
            // Phrase the block message as system-side feedback so small models
            // are less likely to "argue" with it via re-tries.
            let output = if in_plan && PLAN_BLOCKED_TOOLS.contains(&name.as_str()) {
                println!("\x1b[31m[blocked: plan mode]\x1b[0m");
                format!(
                    "[system] Plan mode is active. The '{}' tool is unavailable in this mode. \
                     Do not retry — instead, finish your investigation using read-only tools and \
                     output a Markdown plan. The user will toggle plan mode off (Shift+Tab or /plan) \
                     to execute when they approve the plan.",
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
                    let mut end = 500;
                    while end > 0 && !output.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}\n... ({} chars total)", &output[..end], output.len())
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
                    let mut end = 60;
                    while end > 0 && !repr.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &repr[..end])
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
