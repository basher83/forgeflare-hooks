mod api;
mod session;
mod tools;

use api::{
    classify_error, AgentError, AnthropicClient, ContentBlock, ErrorClass, Message, StopReason,
};
use clap::Parser;
use session::SessionWriter;
use std::io::{self, BufRead, Read as _, Write};
use tools::{all_tool_schemas, dispatch_tool};

const MAX_TOOL_ITERATIONS: usize = 50;
const MAX_RETRIES: usize = 4;
const BACKOFF_SCHEDULE: [u64; 4] = [2, 4, 8, 16];
const RETRY_AFTER_CAP: u64 = 60;
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

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let mut session = SessionWriter::new(&cwd, &cli.model);

    if cli.verbose {
        eprintln!("[verbose] Session ID: {}", session.session_id());
    }

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
            &mut session,
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
                &mut session,
                &input,
            )
            .await;
        }
    }

    session.write_context();
}

async fn run_turn(
    cli: &Cli,
    client: &AnthropicClient,
    system_prompt: &str,
    tools: &[serde_json::Value],
    conversation: &mut Vec<Message>,
    session: &mut SessionWriter,
    input: &str,
) {
    // Add user message
    let user_msg = Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: input.to_string(),
        }],
    };
    conversation.push(user_msg.clone());
    session.append_user_turn(&user_msg);
    session.write_prompt(input);

    // Trim context if needed
    trim_conversation(conversation);

    let mut tool_iterations: usize = 0;
    let mut continuation_count: usize = 0;

    // Inner loop: call API, dispatch tools, repeat
    loop {
        if tool_iterations >= MAX_TOOL_ITERATIONS {
            eprintln!("[warn] Tool iteration limit ({MAX_TOOL_ITERATIONS}) reached");
            recover_conversation(conversation);
            break;
        }

        // Retry loop wrapping the API call
        // attempt 0 = initial call, 1..=MAX_RETRIES = retries
        let mut api_result = None;
        #[allow(clippy::needless_range_loop)]
        for attempt in 0..=MAX_RETRIES {
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

            match result {
                Ok(r) => {
                    api_result = Some(r);
                    break;
                }
                Err(e) => {
                    eprintln!("\n[error] API call failed: {e}");

                    if classify_error(&e) == ErrorClass::Permanent {
                        recover_conversation(conversation);
                        break;
                    }

                    // Transient error — retry if attempts remain
                    if attempt >= MAX_RETRIES {
                        eprintln!("[retry] Max retries ({MAX_RETRIES}) exhausted");
                        recover_conversation(conversation);
                        break;
                    }

                    // Determine delay: retry-after header overrides backoff
                    let delay = if let AgentError::HttpError {
                        retry_after: Some(ra),
                        ..
                    } = &e
                    {
                        let capped = (*ra).min(RETRY_AFTER_CAP);
                        eprintln!("[retry] Using retry-after: {capped}s");
                        capped
                    } else {
                        BACKOFF_SCHEDULE[attempt]
                    };

                    if matches!(e, AgentError::StreamTransient(_)) {
                        eprintln!("[retry] Retrying from beginning of response...");
                    }

                    eprintln!(
                        "[retry] Attempt {}/{MAX_RETRIES}: {} — waiting {delay}s",
                        attempt + 1,
                        e
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                }
            }
        }

        let (blocks, stop_reason, usage) = match api_result {
            Some(r) => r,
            None => break, // All retries failed or permanent error
        };

        // Filter null-input tool_use blocks on MaxTokens truncation
        let blocks = if stop_reason == StopReason::MaxTokens {
            filter_null_input_tool_use(blocks)
        } else {
            blocks
        };

        // Add assistant response to conversation
        let assistant_msg = Message {
            role: "assistant".to_string(),
            content: blocks.clone(),
        };
        conversation.push(assistant_msg.clone());
        session.append_assistant_turn(&assistant_msg, &usage);

        // --- Canonical three-way branch ---

        // 1. EndTurn — normal completion
        if stop_reason == StopReason::EndTurn {
            println!();
            break;
        }

        // 2. MaxTokens — filter, then decide: continue, dispatch tools, or break
        if stop_reason == StopReason::MaxTokens {
            println!();

            match classify_max_tokens(&blocks, continuation_count) {
                MaxTokensAction::BreakEmpty => {
                    eprintln!("[info] Empty response at max_tokens, breaking");
                    break;
                }
                MaxTokensAction::DispatchTools => {
                    // Valid tool_use blocks — fall through to tool dispatch below.
                    // Do NOT increment continuation_count.
                }
                MaxTokensAction::Continue => {
                    continuation_count += 1;
                    eprintln!(
                        "[continue] Response truncated at max_tokens, requesting continuation ({}/{})",
                        continuation_count, MAX_CONTINUATIONS
                    );

                    let cont_msg = Message {
                        role: "user".to_string(),
                        content: vec![ContentBlock::Text {
                            text: "Continue from where you left off.".to_string(),
                        }],
                    };
                    conversation.push(cont_msg.clone());
                    session.append_user_turn(&cont_msg);
                    continue;
                }
                MaxTokensAction::BreakCapReached => {
                    eprintln!("[continue] Max continuations reached, breaking");
                    break;
                }
            }
        }

        // 3. Tool dispatch — runs for both ToolUse and MaxTokens-with-valid-tools
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
                        let display = format_tool_result_display(&output, false, cli.verbose);
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

        let tool_msg = Message {
            role: "user".to_string(),
            content: tool_results,
        };
        conversation.push(tool_msg.clone());
        session.append_user_turn(&tool_msg);

        tool_iterations += 1;
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

const MAX_CONTINUATIONS: usize = 3;

/// Determine what to do after a MaxTokens stop_reason.
/// Returns the action to take given the filtered content blocks and current continuation count.
#[derive(Debug, PartialEq)]
enum MaxTokensAction {
    /// Response was empty (only placeholder) — break immediately
    BreakEmpty,
    /// Valid tool_use blocks present — fall through to tool dispatch
    DispatchTools,
    /// Text-only, under cap — inject continuation prompt
    Continue,
    /// Text-only, cap reached — break
    BreakCapReached,
}

fn classify_max_tokens(blocks: &[ContentBlock], continuation_count: usize) -> MaxTokensAction {
    // Check for empty response (only the "[Response truncated]" placeholder)
    let is_empty = blocks.len() == 1
        && matches!(&blocks[0], ContentBlock::Text { text } if text == "[Response truncated]");
    if is_empty {
        return MaxTokensAction::BreakEmpty;
    }

    let has_valid_tools = blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

    if has_valid_tools {
        MaxTokensAction::DispatchTools
    } else if continuation_count < MAX_CONTINUATIONS {
        MaxTokensAction::Continue
    } else {
        MaxTokensAction::BreakCapReached
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

    #[test]
    fn backoff_schedule_values() {
        assert_eq!(BACKOFF_SCHEDULE, [2, 4, 8, 16]);
        assert_eq!(MAX_RETRIES, 4);
        assert_eq!(RETRY_AFTER_CAP, 60);
    }

    #[test]
    fn permanent_error_skips_retry() {
        // Verify that classify_error returns Permanent for 400
        let e = AgentError::HttpError {
            status: 400,
            retry_after: None,
            body: "bad request".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Permanent);
    }

    #[test]
    fn transient_error_allows_retry() {
        let e = AgentError::HttpError {
            status: 429,
            retry_after: Some(5),
            body: "rate limited".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Transient);
    }

    #[test]
    fn retry_after_cap_applied() {
        // retry_after of 120 should be capped to RETRY_AFTER_CAP (60)
        let ra: u64 = 120;
        let capped = ra.min(RETRY_AFTER_CAP);
        assert_eq!(capped, 60);
    }

    #[test]
    fn retry_after_zero_is_immediate() {
        let ra: u64 = 0;
        let capped = ra.min(RETRY_AFTER_CAP);
        assert_eq!(capped, 0);
    }

    // --- MaxTokens continuation tests ---

    #[test]
    fn max_tokens_text_only_triggers_continuation() {
        let blocks = vec![ContentBlock::Text {
            text: "partial response...".to_string(),
        }];
        assert_eq!(classify_max_tokens(&blocks, 0), MaxTokensAction::Continue);
        assert_eq!(classify_max_tokens(&blocks, 1), MaxTokensAction::Continue);
        assert_eq!(classify_max_tokens(&blocks, 2), MaxTokensAction::Continue);
    }

    #[test]
    fn max_tokens_tool_use_dispatches_tools() {
        let blocks = vec![
            ContentBlock::Text {
                text: "Let me check...".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({"file_path": "/tmp/test"}),
            },
        ];
        // Tool_use MaxTokens falls through to dispatch regardless of continuation_count
        assert_eq!(
            classify_max_tokens(&blocks, 0),
            MaxTokensAction::DispatchTools
        );
        assert_eq!(
            classify_max_tokens(&blocks, 3),
            MaxTokensAction::DispatchTools
        );
    }

    #[test]
    fn max_tokens_cap_enforcement() {
        let blocks = vec![ContentBlock::Text {
            text: "still going...".to_string(),
        }];
        // At count=3 (cap), should break
        assert_eq!(
            classify_max_tokens(&blocks, 3),
            MaxTokensAction::BreakCapReached
        );
        // Beyond cap also breaks
        assert_eq!(
            classify_max_tokens(&blocks, 5),
            MaxTokensAction::BreakCapReached
        );
    }

    #[test]
    fn max_tokens_empty_response_breaks_immediately() {
        // The placeholder produced by filter_null_input_tool_use when all blocks removed
        let blocks = vec![ContentBlock::Text {
            text: "[Response truncated]".to_string(),
        }];
        // Empty breaks regardless of continuation_count
        assert_eq!(classify_max_tokens(&blocks, 0), MaxTokensAction::BreakEmpty);
        assert_eq!(classify_max_tokens(&blocks, 2), MaxTokensAction::BreakEmpty);
    }

    #[test]
    fn max_tokens_tool_use_only_dispatches() {
        // Only tool_use blocks (no text) — still dispatches
        let blocks = vec![ContentBlock::ToolUse {
            id: "tu_1".to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({"command": "ls"}),
        }];
        assert_eq!(
            classify_max_tokens(&blocks, 0),
            MaxTokensAction::DispatchTools
        );
    }

    #[test]
    fn max_continuations_constant() {
        assert_eq!(MAX_CONTINUATIONS, 3);
    }
}
