mod api;
mod tools;

use api::{AgentError, AnthropicClient, ContentBlock, Message, StopReason};
use clap::Parser;
use std::io::{self, BufRead, Read as _, Write};
use tools::{all_tool_schemas, dispatch_tool};

const MAX_TOOL_ITERATIONS: usize = 50;
const CONTEXT_BUDGET_BYTES: usize = 720_000;

#[derive(Parser)]
#[command(
    name = "forgeflare",
    about = "A streaming coding agent powered by Claude"
)]
struct Cli {
    /// Enable verbose debug output
    #[arg(long, default_value_t = false)]
    verbose: bool,

    /// Model to use
    #[arg(long, default_value = "claude-opus-4-6")]
    model: String,

    /// Maximum tokens in response
    #[arg(long, default_value_t = 16384)]
    max_tokens: u32,

    /// API base URL (without /v1/messages path)
    #[arg(
        long,
        env = "ANTHROPIC_API_URL",
        default_value = "https://anthropic-oauth-proxy.tailfb3ea.ts.net"
    )]
    api_url: String,
}

fn build_system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let platform = std::env::consts::OS;

    format!(
        "You are a coding assistant with access to tools for reading, searching, editing files, \
         and running commands.\n\n\
         Environment:\n\
         - Working directory: {cwd}\n\
         - Platform: {platform}\n\n\
         Available tools (use PascalCase names exactly):\n\
         - Read: Read file contents (max 1MB)\n\
         - Glob: List files matching a pattern (max 1000 entries)\n\
         - Bash: Execute shell commands (120s timeout)\n\
         - Edit: Edit files with exact text replacement (max 100KB, use replace_all for bulk)\n\
         - Grep: Search file contents with ripgrep (max 50 matches)\n\n\
         Guidelines:\n\
         - Read files before editing them\n\
         - Use Grep to find code before making changes\n\
         - Prefer targeted edits over full file rewrites\n\
         - Explain what you're doing and why"
    )
}

/// Trim conversation at exchange boundaries to fit within context budget.
/// Preserves the first user message and trims from the front, keeping
/// tool_use/tool_result pairs together.
fn trim_conversation(messages: &mut Vec<Message>) {
    let size: usize = messages
        .iter()
        .map(|m| serde_json::to_string(m).unwrap_or_default().len())
        .sum();

    if size <= CONTEXT_BUDGET_BYTES || messages.len() <= 2 {
        return;
    }

    // Keep first message, trim from front of the rest
    let first = messages.remove(0);
    while messages.len() > 1 {
        let new_size: usize = std::iter::once(&first)
            .chain(messages.iter())
            .map(|m| serde_json::to_string(m).unwrap_or_default().len())
            .sum();
        if new_size <= CONTEXT_BUDGET_BYTES {
            break;
        }
        // Remove pairs to maintain alternation
        messages.remove(0);
        if !messages.is_empty() && messages[0].role == "assistant" {
            messages.remove(0);
        }
    }
    messages.insert(0, first);
}

/// Recover conversation alternation after API errors.
/// Pops trailing User message and any orphaned tool_use to maintain
/// the user/assistant alternation invariant.
fn recover_conversation(messages: &mut Vec<Message>) {
    // Pop trailing user message if present
    if let Some(last) = messages.last() {
        if last.role == "user" {
            messages.pop();
        }
    }
    // Pop trailing assistant message that has only tool_use blocks (orphaned)
    if let Some(last) = messages.last() {
        if last.role == "assistant" {
            let only_tool_use = last
                .content
                .iter()
                .all(|b| matches!(b, ContentBlock::ToolUse { .. }));
            if only_tool_use {
                messages.pop();
                // Also pop the user message before it to maintain alternation
                if let Some(last) = messages.last() {
                    if last.role == "user" {
                        messages.pop();
                    }
                }
            }
        }
    }
}

fn use_color() -> bool {
    std::env::var("NO_COLOR").is_err()
}

/// Filter out null-input tool_use blocks from MaxTokens truncation.
/// Returns the filtered blocks. If all tool_use blocks had null input
/// and nothing remains, returns a vec with a placeholder text block.
fn filter_null_input_tool_use(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let filtered: Vec<ContentBlock> = blocks
        .into_iter()
        .filter(|b| {
            if let ContentBlock::ToolUse { input, .. } = b {
                !input.is_null()
            } else {
                true
            }
        })
        .collect();

    if filtered.is_empty() {
        vec![ContentBlock::Text {
            text: "[Response truncated]".to_string(),
        }]
    } else {
        filtered
    }
}

fn format_tool_result_display(result: &str, is_error: bool, verbose: bool) -> String {
    if is_error {
        let preview = if result.len() > 200 {
            format!("{}...", &result[..result.floor_char_boundary(200)])
        } else {
            result.to_string()
        };
        format!("  Error: {preview}")
    } else if verbose {
        result.to_string()
    } else {
        format!("  ({} chars)", result.len())
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = AnthropicClient::new(&cli.api_url);
    let system_prompt = build_system_prompt();
    let tools = all_tool_schemas();

    if cli.verbose {
        eprintln!("[verbose] API URL: {}", client.api_url());
        eprintln!("[verbose] Model: {}", cli.model);
        eprintln!("[verbose] Max tokens: {}", cli.max_tokens);
        eprintln!(
            "[verbose] API key: {}",
            if client.has_api_key() {
                "present"
            } else {
                "none (OAuth proxy mode)"
            }
        );
    }

    let mut conversation: Vec<Message> = Vec::new();

    // Check for piped stdin
    let is_piped = !atty_check();

    if is_piped {
        // Read entire stdin as single prompt
        let mut input = String::new();
        io::stdin()
            .lock()
            .read_to_string(&mut input)
            .expect("Failed to read stdin");
        let input = input.trim().to_string();
        if input.is_empty() {
            return;
        }
        run_turn(
            &cli,
            &client,
            &system_prompt,
            &tools,
            &mut conversation,
            &input,
        )
        .await;
    } else {
        // Interactive loop
        loop {
            if use_color() {
                eprint!("\x1b[1;34m> \x1b[0m");
            } else {
                eprint!("> ");
            }
            io::stderr().flush().ok();

            let mut input = String::new();
            match io::stdin().lock().read_line(&mut input) {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(e) => {
                    eprintln!("Error reading input: {e}");
                    break;
                }
            }

            let input = input.trim().to_string();
            if input.is_empty() {
                continue;
            }
            if input == "exit" || input == "quit" {
                break;
            }

            run_turn(
                &cli,
                &client,
                &system_prompt,
                &tools,
                &mut conversation,
                &input,
            )
            .await;
        }
    }
}

async fn run_turn(
    cli: &Cli,
    client: &AnthropicClient,
    system_prompt: &str,
    tools: &[serde_json::Value],
    conversation: &mut Vec<Message>,
    input: &str,
) {
    // Add user message
    conversation.push(Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: input.to_string(),
        }],
    });

    // Trim context if needed
    trim_conversation(conversation);

    let mut tool_iterations: usize = 0;

    // Inner loop: call API, dispatch tools, repeat
    loop {
        if tool_iterations >= MAX_TOOL_ITERATIONS {
            eprintln!("[warn] Tool iteration limit ({MAX_TOOL_ITERATIONS}) reached");
            recover_conversation(conversation);
            break;
        }

        let result = client
            .send_message(
                &cli.model,
                cli.max_tokens,
                system_prompt,
                conversation,
                tools,
                &mut |text| {
                    print!("{text}");
                    io::stdout().flush().ok();
                },
            )
            .await;

        let (blocks, stop_reason) = match result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("\n[error] API call failed: {e}");
                if let AgentError::HttpError {
                    retry_after: Some(ra),
                    ..
                } = &e
                {
                    eprintln!("[info] Retry-After: {ra}s");
                }
                recover_conversation(conversation);
                break;
            }
        };

        // Filter null-input tool_use blocks for MaxTokens truncation
        let blocks = if stop_reason == StopReason::MaxTokens {
            filter_null_input_tool_use(blocks)
        } else {
            blocks
        };

        // Add assistant response to conversation
        conversation.push(Message {
            role: "assistant".to_string(),
            content: blocks.clone(),
        });

        match stop_reason {
            StopReason::EndTurn => {
                println!();
                break;
            }
            StopReason::MaxTokens => {
                println!();
                eprintln!("[info] Response truncated (max_tokens)");
                break;
            }
            StopReason::ToolUse => {
                // Dispatch tools
                let mut tool_results: Vec<ContentBlock> = Vec::new();

                for block in &blocks {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        if input.is_null() {
                            continue;
                        }

                        if cli.verbose {
                            eprintln!("\n[tool] {name}({})", truncate_json(input, 100));
                        } else {
                            eprintln!("\n[tool] {name}");
                        }

                        let result = dispatch_tool(name, input, &mut |text| {
                            if cli.verbose {
                                eprint!("{text}");
                            }
                        });

                        let (content, is_error) = match result {
                            Ok(output) => {
                                let display =
                                    format_tool_result_display(&output, false, cli.verbose);
                                eprintln!("{display}");
                                (output, false)
                            }
                            Err(err) => {
                                let display = format_tool_result_display(&err, true, cli.verbose);
                                eprintln!("{display}");
                                (err, true)
                            }
                        };

                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content,
                            is_error: if is_error { Some(true) } else { None },
                        });
                    }
                }

                if tool_results.is_empty() {
                    break;
                }

                conversation.push(Message {
                    role: "user".to_string(),
                    content: tool_results,
                });

                tool_iterations += 1;
            }
        }
    }
}

fn truncate_json(value: &serde_json::Value, max_len: usize) -> String {
    let s = value.to_string();
    if s.len() <= max_len {
        s
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max_len)])
    }
}

/// Check if stdin is a terminal (not piped).
fn atty_check() -> bool {
    std::io::IsTerminal::is_terminal(&io::stdin())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_contains_environment_info() {
        let prompt = build_system_prompt();
        assert!(prompt.contains("Working directory:"));
        assert!(prompt.contains("Platform:"));
        // Check PascalCase tool names
        assert!(prompt.contains("Read:"));
        assert!(prompt.contains("Glob:"));
        assert!(prompt.contains("Bash:"));
        assert!(prompt.contains("Edit:"));
        assert!(prompt.contains("Grep:"));
    }

    #[test]
    fn trim_conversation_under_budget() {
        let mut msgs = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "hello".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            },
        ];
        let original_len = msgs.len();
        trim_conversation(&mut msgs);
        assert_eq!(msgs.len(), original_len);
    }

    #[test]
    fn recover_conversation_pops_trailing_user() {
        let mut msgs = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "hello".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "more".to_string(),
                }],
            },
        ];
        recover_conversation(&mut msgs);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs.last().unwrap().role, "assistant");
    }

    #[test]
    fn filter_null_input_removes_null_tool_use() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".to_string(),
            },
            ContentBlock::ToolUse {
                id: "id1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::Value::Null,
            },
        ];
        let filtered = filter_null_input_tool_use(blocks);
        assert_eq!(filtered.len(), 1);
        assert!(matches!(filtered[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn filter_null_input_placeholder_when_empty() {
        let blocks = vec![ContentBlock::ToolUse {
            id: "id1".to_string(),
            name: "Bash".to_string(),
            input: serde_json::Value::Null,
        }];
        let filtered = filter_null_input_tool_use(blocks);
        assert_eq!(filtered.len(), 1);
        if let ContentBlock::Text { text } = &filtered[0] {
            assert!(text.contains("truncated"));
        } else {
            panic!("expected placeholder Text block");
        }
    }

    #[test]
    fn tool_result_display_error_preview() {
        let long_error = "x".repeat(300);
        let display = format_tool_result_display(&long_error, true, false);
        assert!(display.contains("Error:"));
        assert!(display.contains("..."));
        assert!(display.len() < 250);
    }

    #[test]
    fn tool_result_display_size_non_verbose() {
        let result = "hello world";
        let display = format_tool_result_display(result, false, false);
        assert!(display.contains("11 chars"));
    }

    #[test]
    fn truncate_json_short() {
        let val = serde_json::json!({"key": "val"});
        let s = truncate_json(&val, 100);
        assert!(!s.contains("..."));
    }

    #[test]
    fn truncate_json_long() {
        let val = serde_json::json!({"key": "a very long value that exceeds the truncation limit"});
        let s = truncate_json(&val, 20);
        assert!(s.contains("..."));
    }
}
