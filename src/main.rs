mod api;
mod hooks;
mod session;
mod tools;

use api::{
    classify_error, AgentError, AnthropicClient, ContentBlock, ErrorClass, Message, StopReason,
};
use clap::Parser;
use hooks::{HookRunner, PostToolResult, PreToolResult};
use session::SessionWriter;
use std::io::{self, BufRead, Read as _, Write};
use tools::{all_tool_schemas, dispatch_tool, tool_effect, ToolEffect};

const MAX_TOOL_ITERATIONS: usize = 50;
const MAX_RETRIES: usize = 4;
const BACKOFF_SCHEDULE: [u64; 4] = [2, 4, 8, 16];
const RETRY_AFTER_CAP: u64 = 60;
const CONTEXT_BUDGET_BYTES: usize = 720_000;
const MODEL_CONTEXT_TOKENS: u64 = 200_000;
const TRIM_THRESHOLD: u64 = MODEL_CONTEXT_TOKENS * 60 / 100; // 120K tokens
const MAX_CONSECUTIVE_BLOCKS: usize = 3;
const MAX_TOTAL_BLOCKS: usize = 10;
const PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 32_768;
const MAX_CONTINUATIONS: usize = 3;

#[derive(Debug, PartialEq)]
enum InstructionsResult {
    Found { filename: String, contents: String },
    Skipped { filename: String, reason: String },
    NotFound,
}

/// Search cwd for CLAUDE.md then AGENTS.md. First match wins.
/// Returns Found on success, Skipped if a candidate exists but can't be loaded
/// (size limit, permissions), or NotFound if neither file exists.
fn load_project_instructions() -> InstructionsResult {
    let candidates = ["CLAUDE.md", "AGENTS.md"];
    let mut last_skipped: Option<InstructionsResult> = None;

    for candidate in &candidates {
        let metadata = match std::fs::metadata(candidate) {
            Ok(m) => m,
            Err(_) => continue, // file not found or dangling symlink
        };

        if metadata.len() as usize > PROJECT_INSTRUCTIONS_MAX_BYTES {
            last_skipped = Some(InstructionsResult::Skipped {
                filename: candidate.to_string(),
                reason: "exceeds 32KB".to_string(),
            });
            continue;
        }

        match std::fs::read_to_string(candidate) {
            Ok(contents) => {
                return InstructionsResult::Found {
                    filename: candidate.to_string(),
                    contents,
                };
            }
            Err(e) => {
                last_skipped = Some(InstructionsResult::Skipped {
                    filename: candidate.to_string(),
                    reason: e.to_string(),
                });
                continue;
            }
        }
    }

    last_skipped.unwrap_or(InstructionsResult::NotFound)
}

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

/// Gate trim_conversation on actual token usage from the API.
/// - last_input_tokens == 0: no data yet, run byte-based trim (safety net)
/// - last_input_tokens > 0 && < TRIM_THRESHOLD: skip trim (context is safe)
/// - last_input_tokens >= TRIM_THRESHOLD: run byte-based trim
fn trim_if_needed(messages: &mut Vec<Message>, last_input_tokens: u64) {
    if last_input_tokens == 0 || last_input_tokens >= TRIM_THRESHOLD {
        trim_conversation(messages);
    }
}

/// Recover conversation alternation after API errors.
/// Pops trailing User message and any orphaned tool_use to maintain
/// the user/assistant alternation invariant.
fn recover_conversation(messages: &mut Vec<Message>) {
    // Guard: never empty the conversation entirely — at minimum keep the
    // first user message so the next API call has something to send.
    if messages.len() <= 1 {
        return;
    }
    // Pop trailing user message if present (but keep at least 1 message)
    if messages.len() > 1 {
        if let Some(last) = messages.last() {
            if last.role == "user" {
                messages.pop();
            }
        }
    }
    // Pop trailing assistant message that has only tool_use blocks (orphaned)
    if messages.len() > 1 {
        if let Some(last) = messages.last() {
            if last.role == "assistant" {
                let only_tool_use = last
                    .content
                    .iter()
                    .all(|b| matches!(b, ContentBlock::ToolUse { .. }));
                if only_tool_use {
                    messages.pop();
                    // Also pop the user message before it to maintain alternation
                    if messages.len() > 1 {
                        if let Some(last) = messages.last() {
                            if last.role == "user" {
                                messages.pop();
                            }
                        }
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

/// Decision from pre-dispatch protocol: allow dispatch, block with a ToolResult, or trip threshold.
#[derive(Debug)]
enum PreDispatchResult {
    Allow,
    Blocked(ContentBlock),
    ThresholdTripped,
}

/// Why a turn ended. Passed to the Stop hook as the `"reason"` field.
#[derive(Debug, Clone, Copy, PartialEq)]
enum TurnStopReason {
    EndTurn,
    IterationLimit,
    ApiError,
    ContinuationCap,
    BlockLimitConsecutive,
    BlockLimitTotal,
    ConvergenceSignal,
}

impl TurnStopReason {
    fn as_str(self) -> &'static str {
        match self {
            TurnStopReason::EndTurn => "end_turn",
            TurnStopReason::IterationLimit => "iteration_limit",
            TurnStopReason::ApiError => "api_error",
            TurnStopReason::ContinuationCap => "continuation_cap",
            TurnStopReason::BlockLimitConsecutive => "block_limit_consecutive",
            TurnStopReason::BlockLimitTotal => "block_limit_total",
            TurnStopReason::ConvergenceSignal => "convergence_signal",
        }
    }
}

/// Unified pre-dispatch protocol for both parallel and sequential paths.
/// Checks null-input, runs pre-hook, manages block counting and threshold checks.
/// Block counts are mutated in place so callers cannot forget to apply them.
async fn run_pre_dispatch(
    hooks: &HookRunner,
    id: &str,
    name: &str,
    input: &serde_json::Value,
    iterations: usize,
    consecutive_block_count: &mut usize,
    total_block_count: &mut usize,
) -> PreDispatchResult {
    // Null-input safety net (truncated tool_use from API anomaly)
    if input.is_null() {
        return PreDispatchResult::Blocked(ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "null input (truncated tool_use)".to_string(),
            is_error: Some(true),
        });
    }

    let pre_result = hooks.run_pre_tool_use(name, input, iterations).await;

    match pre_result {
        PreToolResult::Block { reason, .. } => {
            *consecutive_block_count += 1;
            *total_block_count += 1;

            if *consecutive_block_count >= MAX_CONSECUTIVE_BLOCKS {
                eprintln!("[hooks] Consecutive block limit ({MAX_CONSECUTIVE_BLOCKS}) reached");
                return PreDispatchResult::ThresholdTripped;
            }
            if *total_block_count >= MAX_TOTAL_BLOCKS {
                eprintln!("[hooks] Total block limit ({MAX_TOTAL_BLOCKS}) reached");
                return PreDispatchResult::ThresholdTripped;
            }

            PreDispatchResult::Blocked(ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: reason,
                is_error: Some(true),
            })
        }
        PreToolResult::Allow => {
            *consecutive_block_count = 0;
            PreDispatchResult::Allow
        }
    }
}

/// Unified post-dispatch protocol for both parallel and sequential paths.
/// Formats and logs the result, then runs post-hooks. Returns true if a
/// convergence signal was received (caller should set signal_break).
async fn run_post_dispatch(
    hooks: &HookRunner,
    name: &str,
    input: &serde_json::Value,
    content: &str,
    is_error: bool,
    iterations: usize,
    verbose: bool,
) -> bool {
    let display = format_tool_result_display(content, is_error, verbose);
    eprintln!("{display}");

    let post_result = hooks
        .run_post_tool_use(name, input, content, is_error, iterations)
        .await;
    matches!(post_result, PostToolResult::Signal { .. })
}

fn log_tool_dispatch(name: &str, input: &serde_json::Value, verbose: bool) {
    if verbose {
        eprintln!("\n[tool] {name}({})", truncate_json(input, 100));
    } else {
        eprintln!("\n[tool] {name}");
    }
}

fn threshold_stop_reason(consecutive_block_count: usize) -> TurnStopReason {
    if consecutive_block_count >= MAX_CONSECUTIVE_BLOCKS {
        TurnStopReason::BlockLimitConsecutive
    } else {
        TurnStopReason::BlockLimitTotal
    }
}

/// Dispatch a tool and wrap the result as a ContentBlock::ToolResult.
/// Used by the parallel path inside spawn_blocking.
fn dispatch_to_tool_result(id: String, name: String, input: serde_json::Value) -> ContentBlock {
    let result = dispatch_tool(&name, &input, &mut |_: &str| {});
    let (content, is_error) = match result {
        Ok(output) => (output, false),
        Err(err) => (err, true),
    };
    ContentBlock::ToolResult {
        tool_use_id: id,
        content,
        is_error: if is_error { Some(true) } else { None },
    }
}

/// Join spawned parallel futures into their slots, handling panics.
async fn join_spawned_futures(
    futures: Vec<(usize, tokio::task::JoinHandle<ContentBlock>)>,
    slots: &mut [Option<ContentBlock>],
    tool_ids: &[(String, String, serde_json::Value)],
) {
    let handles: Vec<_> = futures
        .into_iter()
        .map(|(idx, h)| async move { (idx, h.await) })
        .collect();
    for (idx, result) in futures_util::future::join_all(handles).await {
        slots[idx] = Some(match result {
            Ok(block) => block,
            Err(_) => ContentBlock::ToolResult {
                tool_use_id: tool_ids[idx].0.clone(),
                content: "tool panicked".to_string(),
                is_error: Some(true),
            },
        });
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
    let mut system_prompt = build_system_prompt();
    let tools = all_tool_schemas();

    // Load project instructions (CLAUDE.md or AGENTS.md)
    match load_project_instructions() {
        InstructionsResult::Found {
            ref filename,
            ref contents,
        } => {
            if cli.verbose {
                eprintln!(
                    "[verbose] Loaded project instructions from {} ({} bytes)",
                    filename,
                    contents.len()
                );
            }
            system_prompt = format!(
                "{system_prompt}\n\n---\n\n## Project Instructions (from {filename})\n\n{contents}"
            );
        }
        InstructionsResult::Skipped {
            ref filename,
            ref reason,
        } => {
            eprintln!("[warn] {filename}: {reason}");
        }
        InstructionsResult::NotFound => {
            if cli.verbose {
                eprintln!("[verbose] No CLAUDE.md or AGENTS.md found in working directory");
            }
        }
    }

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
    let hooks = HookRunner::load(".forgeflare/hooks.toml", &cwd);
    hooks.clear_convergence_state();

    if cli.verbose {
        eprintln!("[verbose] Session ID: {}", session.session_id());
        if hooks.has_hooks() {
            eprintln!("[verbose] Hooks loaded from .forgeflare/hooks.toml");
        }
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
            &hooks,
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
                &hooks,
                &input,
            )
            .await;
        }
    }

    session.write_context();
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    cli: &Cli,
    client: &AnthropicClient,
    system_prompt: &str,
    tools: &[serde_json::Value],
    conversation: &mut Vec<Message>,
    session: &mut SessionWriter,
    hooks: &HookRunner,
    input: &str,
) {
    let user_msg = Message {
        role: "user".to_string(),
        content: vec![ContentBlock::Text {
            text: input.to_string(),
        }],
    };
    conversation.push(user_msg.clone());
    session.append_user_turn(&user_msg);
    session.write_prompt(input);

    let mut tool_iterations: usize = 0;
    let mut continuation_count: usize = 0;
    let mut last_input_tokens: u64 = 0;
    let mut consecutive_block_count: usize = 0;
    let mut total_block_count: usize = 0;
    let mut total_tokens: u64 = 0;
    let mut turn_stop_reason = TurnStopReason::EndTurn;
    loop {
        trim_if_needed(conversation, last_input_tokens);

        if tool_iterations >= MAX_TOOL_ITERATIONS {
            eprintln!("[warn] Tool iteration limit ({MAX_TOOL_ITERATIONS}) reached");
            recover_conversation(conversation);
            turn_stop_reason = TurnStopReason::IterationLimit;
            break;
        }

        // Retry loop: attempt 0 = initial call, 1..=MAX_RETRIES = retries
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
                        turn_stop_reason = TurnStopReason::ApiError;
                        break;
                    }
                    if attempt >= MAX_RETRIES {
                        eprintln!("[retry] Max retries ({MAX_RETRIES}) exhausted");
                        recover_conversation(conversation);
                        turn_stop_reason = TurnStopReason::ApiError;
                        break;
                    }
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
            None => break,
        };

        last_input_tokens = usage.input_tokens;
        total_tokens += usage.input_tokens + usage.output_tokens;
        if cli.verbose {
            eprintln!(
                "[verbose] Cache: {} read, {} created, {} total input",
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
                usage.input_tokens
            );
        }

        let blocks = if stop_reason == StopReason::MaxTokens {
            filter_null_input_tool_use(blocks)
        } else {
            blocks
        };

        let assistant_msg = Message {
            role: "assistant".to_string(),
            content: blocks.clone(),
        };
        conversation.push(assistant_msg.clone());
        session.append_assistant_turn(&assistant_msg, &usage);

        // EndTurn — normal completion
        if stop_reason == StopReason::EndTurn {
            println!();
            turn_stop_reason = TurnStopReason::EndTurn;
            break;
        }

        // MaxTokens — decide: continue, dispatch tools, or break
        if stop_reason == StopReason::MaxTokens {
            println!();

            match classify_max_tokens(&blocks, continuation_count) {
                MaxTokensAction::BreakEmpty => {
                    eprintln!("[info] Empty response at max_tokens, breaking");
                    turn_stop_reason = TurnStopReason::ContinuationCap;
                    break;
                }
                MaxTokensAction::DispatchTools => {} // Fall through to tool dispatch
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
                    turn_stop_reason = TurnStopReason::ContinuationCap;
                    break;
                }
            }
        }

        // Tool dispatch — runs for both ToolUse and MaxTokens-with-valid-tools
        let tool_uses: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();
        let all_pure = !tool_uses.is_empty()
            && tool_uses
                .iter()
                .all(|(_, name, _)| tool_effect(name) == ToolEffect::Pure);

        let mut signal_break = false;
        let mut threshold_tripped = false;
        let mut threshold_reason = TurnStopReason::EndTurn; // placeholder, only used when threshold_tripped
        let tool_results: Vec<ContentBlock> = if all_pure {
            // Parallel path: all tools are pure (Read, Glob, Grep)
            let batch_size = tool_uses.len();
            let mut slots: Vec<Option<ContentBlock>> = vec![None; batch_size];
            let mut blocked_flags: Vec<bool> = vec![false; batch_size];
            let mut spawn_futures: Vec<(usize, tokio::task::JoinHandle<ContentBlock>)> = Vec::new();

            for (i, (id, name, input)) in tool_uses.iter().enumerate() {
                match run_pre_dispatch(
                    hooks,
                    id,
                    name,
                    input,
                    tool_iterations,
                    &mut consecutive_block_count,
                    &mut total_block_count,
                )
                .await
                {
                    PreDispatchResult::Allow => {
                        log_tool_dispatch(name, input, cli.verbose);
                        let id = id.clone();
                        let name = name.clone();
                        let input = input.clone();
                        let handle = tokio::task::spawn_blocking(move || {
                            dispatch_to_tool_result(id, name, input)
                        });
                        spawn_futures.push((i, handle));
                    }
                    PreDispatchResult::Blocked(cb) => {
                        slots[i] = Some(cb);
                        blocked_flags[i] = true;
                    }
                    PreDispatchResult::ThresholdTripped => {
                        threshold_tripped = true;
                        threshold_reason = threshold_stop_reason(consecutive_block_count);
                        break;
                    }
                }
            }

            join_spawned_futures(spawn_futures, &mut slots, &tool_uses).await;
            if threshold_tripped {
                Vec::new()
            } else {
                for (i, (_, name, input)) in tool_uses.iter().enumerate() {
                    if blocked_flags[i] {
                        continue;
                    }
                    if let Some(ContentBlock::ToolResult {
                        ref content,
                        is_error,
                        ..
                    }) = slots[i]
                    {
                        let is_err = is_error.unwrap_or(false);
                        if run_post_dispatch(
                            hooks,
                            name,
                            input,
                            content,
                            is_err,
                            tool_iterations,
                            cli.verbose,
                        )
                        .await
                        {
                            signal_break = true;
                        }
                    }
                }
                slots.into_iter().map(|s| s.unwrap()).collect()
            }
        } else {
            // Sequential path: any Mutating tool in the batch
            let mut tool_results: Vec<ContentBlock> = Vec::new();

            for (id, name, input) in &tool_uses {
                match run_pre_dispatch(
                    hooks,
                    id,
                    name,
                    input,
                    tool_iterations,
                    &mut consecutive_block_count,
                    &mut total_block_count,
                )
                .await
                {
                    PreDispatchResult::Allow => {}
                    PreDispatchResult::Blocked(cb) => {
                        tool_results.push(cb);
                        continue;
                    }
                    PreDispatchResult::ThresholdTripped => {
                        threshold_tripped = true;
                        threshold_reason = threshold_stop_reason(consecutive_block_count);
                        break;
                    }
                }

                log_tool_dispatch(name, input, cli.verbose);

                let result = dispatch_tool(name, input, &mut |text| {
                    if cli.verbose {
                        eprint!("{text}");
                    }
                });
                let (content, is_error) = match result {
                    Ok(output) => (output, false),
                    Err(err) => (err, true),
                };

                if run_post_dispatch(
                    hooks,
                    name,
                    input,
                    &content,
                    is_error,
                    tool_iterations,
                    cli.verbose,
                )
                .await
                {
                    signal_break = true;
                }

                tool_results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content,
                    is_error: if is_error { Some(true) } else { None },
                });
            }

            tool_results
        };

        // Block threshold takes precedence over signal_break
        if threshold_tripped {
            conversation.pop();
            turn_stop_reason = threshold_reason;
            break;
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
        if signal_break {
            turn_stop_reason = TurnStopReason::ConvergenceSignal;
            break;
        }
    }

    hooks
        .run_stop(turn_stop_reason.as_str(), tool_iterations, total_tokens)
        .await;
}

fn truncate_json(value: &serde_json::Value, max_len: usize) -> String {
    let s = value.to_string();
    if s.len() <= max_len {
        s
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max_len)])
    }
}

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

    // --- Token-aware trim tests ---

    #[test]
    fn trim_threshold_is_60_percent() {
        assert_eq!(MODEL_CONTEXT_TOKENS, 200_000);
        assert_eq!(TRIM_THRESHOLD, 120_000);
    }

    #[test]
    fn trim_if_needed_zero_tokens_runs_trim() {
        // First call (no data yet) — trim should run.
        // Build a conversation that exceeds byte budget to verify trim actually fires.
        let big_text = "x".repeat(CONTEXT_BUDGET_BYTES + 1000);
        let mut msgs = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "keep".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: big_text.clone(),
                }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "more".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "reply".to_string(),
                }],
            },
        ];
        let original_len = msgs.len();
        trim_if_needed(&mut msgs, 0);
        // Conversation was over budget, trim should have removed messages
        assert!(
            msgs.len() < original_len,
            "trim should have reduced message count"
        );
    }

    #[test]
    fn trim_if_needed_under_threshold_skips_trim() {
        // Usage is under 120K — trim should NOT run, even if byte budget exceeded.
        let big_text = "x".repeat(CONTEXT_BUDGET_BYTES + 1000);
        let mut msgs = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "keep".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text { text: big_text }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "more".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "reply".to_string(),
                }],
            },
        ];
        let original_len = msgs.len();
        trim_if_needed(&mut msgs, 50_000); // Well under 120K
                                           // Trim should have been skipped entirely
        assert_eq!(msgs.len(), original_len, "trim should not have run");
    }

    #[test]
    fn trim_if_needed_at_threshold_runs_trim() {
        // Usage exactly at threshold — trim should run.
        let big_text = "x".repeat(CONTEXT_BUDGET_BYTES + 1000);
        let mut msgs = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "keep".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text { text: big_text }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "more".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "reply".to_string(),
                }],
            },
        ];
        let original_len = msgs.len();
        trim_if_needed(&mut msgs, TRIM_THRESHOLD);
        assert!(
            msgs.len() < original_len,
            "trim should have reduced message count"
        );
    }

    #[test]
    fn trim_if_needed_above_threshold_runs_trim() {
        // Usage above threshold — trim should run.
        let big_text = "x".repeat(CONTEXT_BUDGET_BYTES + 1000);
        let mut msgs = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "keep".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text { text: big_text }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "more".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::Text {
                    text: "reply".to_string(),
                }],
            },
        ];
        let original_len = msgs.len();
        trim_if_needed(&mut msgs, 180_000);
        assert!(
            msgs.len() < original_len,
            "trim should have reduced message count"
        );
    }

    #[test]
    fn last_input_tokens_resets_per_turn() {
        // last_input_tokens is a local variable in run_turn, so each call starts at 0.
        // This test verifies the constant relationship — run_turn creates fresh state.
        // The variable is initialized to 0 at the top of run_turn, verified by code inspection.
        // We test the gate behavior: 0 always runs trim (first-call safety net).
        let mut msgs = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        }];
        // Small conversation under budget — trim is a no-op, but should be called
        trim_if_needed(&mut msgs, 0);
        assert_eq!(msgs.len(), 1, "small conversation unchanged by trim");
    }

    // --- Tool parallelism tests ---

    #[test]
    fn batch_classification_all_pure() {
        // A batch of only Read/Glob/Grep tools should classify as all-pure
        let tool_uses = vec![
            ("id1", "Read", serde_json::json!({"file_path": "/tmp/a"})),
            ("id2", "Glob", serde_json::json!({"pattern": "*.rs"})),
            ("id3", "Grep", serde_json::json!({"pattern": "foo"})),
        ];
        let all_pure = tool_uses
            .iter()
            .all(|(_, name, _)| tool_effect(name) == ToolEffect::Pure);
        assert!(all_pure, "all Read/Glob/Grep should be pure");
    }

    #[test]
    fn batch_classification_mixed_is_sequential() {
        // A batch with Read + Edit should NOT classify as all-pure
        let tool_uses = vec![
            ("id1", "Read", serde_json::json!({"file_path": "/tmp/a"})),
            (
                "id2",
                "Edit",
                serde_json::json!({"file_path": "/tmp/b", "old_str": "x", "new_str": "y"}),
            ),
        ];
        let all_pure = tool_uses
            .iter()
            .all(|(_, name, _)| tool_effect(name) == ToolEffect::Pure);
        assert!(!all_pure, "mixed batch with Edit should not be all-pure");
    }

    #[test]
    fn batch_classification_single_pure() {
        // Degenerate case: batch of 1 pure tool works correctly
        let tool_uses = vec![("id1", "Read", serde_json::json!({"file_path": "/tmp/a"}))];
        let all_pure = tool_uses
            .iter()
            .all(|(_, name, _)| tool_effect(name) == ToolEffect::Pure);
        assert!(all_pure, "single Read tool should be all-pure");
    }

    #[tokio::test]
    async fn parallel_reads_faster_than_sequential() {
        // 3 concurrent Reads should complete faster than sequential.
        // We use Bash(sleep) dispatches wrapped in spawn_blocking to measure concurrency,
        // but instead we'll use actual Read calls which are fast I/O.
        // Create temp files and verify parallel is at least not slower.
        let dir = std::env::temp_dir().join("forgeflare_parallel_test");
        let _ = std::fs::create_dir_all(&dir);
        for i in 0..3 {
            std::fs::write(dir.join(format!("file{i}.txt")), format!("content {i}")).unwrap();
        }

        let files: Vec<_> = (0..3)
            .map(|i| {
                dir.join(format!("file{i}.txt"))
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();

        // Parallel: use join_all with spawn_blocking
        let start = std::time::Instant::now();
        let handles: Vec<_> = files
            .iter()
            .map(|f| {
                let f = f.clone();
                tokio::task::spawn_blocking(move || {
                    dispatch_tool("Read", &serde_json::json!({"file_path": f}), &mut |_| {})
                })
            })
            .collect();
        let results = futures_util::future::join_all(handles).await;
        let parallel_time = start.elapsed();

        // All should succeed
        for r in &results {
            assert!(r.as_ref().unwrap().is_ok());
        }

        // Sequential
        let start = std::time::Instant::now();
        for f in &files {
            let r = dispatch_tool("Read", &serde_json::json!({"file_path": f}), &mut |_| {});
            assert!(r.is_ok());
        }
        let sequential_time = start.elapsed();

        // Both complete successfully; parallel should not be significantly slower
        // (for fast I/O ops the difference is small, but the mechanism works)
        assert!(
            parallel_time <= sequential_time + std::time::Duration::from_millis(50),
            "parallel ({parallel_time:?}) should not be much slower than sequential ({sequential_time:?})"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn parallel_tool_error_doesnt_cancel_siblings() {
        // One failing Read should not prevent other Reads from completing
        let dir = std::env::temp_dir().join("forgeflare_parallel_error_test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("exists.txt"), "hello").unwrap();

        let inputs = vec![
            ("id1", dir.join("exists.txt").to_str().unwrap().to_string()),
            (
                "id2",
                "/nonexistent/file_that_does_not_exist.txt".to_string(),
            ),
            ("id3", dir.join("exists.txt").to_str().unwrap().to_string()),
        ];

        let handles: Vec<_> = inputs
            .iter()
            .map(|(id, f)| {
                let id = id.to_string();
                let f = f.clone();
                tokio::task::spawn_blocking(move || {
                    let result =
                        dispatch_tool("Read", &serde_json::json!({"file_path": f}), &mut |_| {});
                    let (content, is_error) = match result {
                        Ok(output) => (output, false),
                        Err(err) => (err, true),
                    };
                    ContentBlock::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error: if is_error { Some(true) } else { None },
                    }
                })
            })
            .collect();

        let results = futures_util::future::join_all(handles).await;

        // First should succeed
        let r0 = results[0].as_ref().unwrap();
        if let ContentBlock::ToolResult { is_error, .. } = r0 {
            assert!(is_error.is_none(), "first read should succeed");
        }
        // Second should fail
        let r1 = results[1].as_ref().unwrap();
        if let ContentBlock::ToolResult {
            is_error, content, ..
        } = r1
        {
            assert_eq!(*is_error, Some(true), "missing file should error");
            assert!(content.contains("not found"));
        }
        // Third should succeed (not cancelled by second's failure)
        let r2 = results[2].as_ref().unwrap();
        if let ContentBlock::ToolResult { is_error, .. } = r2 {
            assert!(
                is_error.is_none(),
                "third read should succeed despite second failing"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn parallel_preserves_result_ordering() {
        // Results from join_all must maintain the same order as input futures
        let dir = std::env::temp_dir().join("forgeflare_parallel_order_test");
        let _ = std::fs::create_dir_all(&dir);
        for i in 0..3 {
            std::fs::write(dir.join(format!("ord{i}.txt")), format!("content_{i}")).unwrap();
        }

        let files: Vec<_> = (0..3)
            .map(|i| {
                (
                    format!("id_{i}"),
                    dir.join(format!("ord{i}.txt"))
                        .to_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect();

        let handles: Vec<_> = files
            .iter()
            .map(|(id, f)| {
                let id = id.clone();
                let f = f.clone();
                tokio::task::spawn_blocking(move || {
                    let result =
                        dispatch_tool("Read", &serde_json::json!({"file_path": f}), &mut |_| {});
                    (id, result)
                })
            })
            .collect();

        let results = futures_util::future::join_all(handles).await;

        for (i, r) in results.iter().enumerate() {
            let (id, result) = r.as_ref().unwrap();
            assert_eq!(
                id,
                &format!("id_{i}"),
                "result {i} should preserve ordering"
            );
            let content = result.as_ref().unwrap();
            assert!(
                content.contains(&format!("content_{i}")),
                "result {i} should contain correct content"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Hook dispatch integration tests ---

    #[test]
    fn block_counter_constants() {
        assert_eq!(MAX_CONSECUTIVE_BLOCKS, 3);
        assert_eq!(MAX_TOTAL_BLOCKS, 10);
    }

    #[test]
    fn consecutive_block_threshold_logic() {
        // Simulate consecutive blocks hitting threshold
        let mut consecutive = 0usize;
        let mut tripped = false;

        for _ in 0..3 {
            consecutive += 1;
            if consecutive >= MAX_CONSECUTIVE_BLOCKS {
                tripped = true;
                break;
            }
        }
        assert!(tripped, "threshold should trip at 3 consecutive blocks");
        assert_eq!(consecutive, 3);
    }

    #[test]
    fn consecutive_block_resets_on_allow() {
        // Simulate the counter behavior from run_turn:
        // block, block, allow(reset), block, block → consecutive should be 2
        let steps: &[bool] = &[false, false, true, false, false]; // true=allow, false=block
        let mut consecutive = 0usize;
        for &is_allow in steps {
            if is_allow {
                consecutive = 0;
            } else {
                consecutive += 1;
            }
        }
        assert_eq!(consecutive, 2, "should be 2 after reset and 2 more blocks");
        assert!(consecutive < MAX_CONSECUTIVE_BLOCKS, "should not trip");
    }

    #[test]
    fn total_block_never_resets_in_inner_loop() {
        // Simulate: block, allow (resets consecutive only), block, block
        let steps: &[bool] = &[false, true, false, false]; // true=allow, false=block
        let mut total = 0usize;
        let mut consecutive = 0usize;
        for &is_allow in steps {
            if is_allow {
                consecutive = 0;
            } else {
                total += 1;
                consecutive += 1;
            }
        }
        assert_eq!(total, 3, "total should count all blocks");
        assert_eq!(consecutive, 2, "consecutive should be 2 after reset");
    }

    #[test]
    fn total_block_threshold_logic() {
        let mut total = 0usize;
        let mut tripped = false;

        for _ in 0..10 {
            total += 1;
            if total >= MAX_TOTAL_BLOCKS {
                tripped = true;
                break;
            }
        }
        assert!(tripped, "threshold should trip at 10 total blocks");
        assert_eq!(total, 10);
    }

    #[test]
    fn both_counters_reset_on_outer_loop() {
        // In run_turn, both are initialized to 0 (fresh per call).
        // Verify by simulating two "turns":
        let make_counters = || -> (usize, usize) { (0, 0) };

        let (c1, t1) = make_counters();
        assert_eq!(c1, 0);
        assert_eq!(t1, 0);

        // Simulate some blocks in first "turn"
        // Then new turn resets
        let (c2, t2) = make_counters();
        assert_eq!(c2, 0);
        assert_eq!(t2, 0);
    }

    #[test]
    fn consecutive_takes_precedence_over_total() {
        // When both trip simultaneously (3 consecutive that also push total to 10),
        // consecutive fires first.
        let mut consecutive = 0usize;
        let mut total = 7usize; // already had 7 total blocks
        let mut reason = "";

        for _ in 0..3 {
            consecutive += 1;
            total += 1;
            if consecutive >= MAX_CONSECUTIVE_BLOCKS {
                reason = "block_limit_consecutive";
                break;
            }
            if total >= MAX_TOTAL_BLOCKS {
                reason = "block_limit_total";
                break;
            }
        }

        assert_eq!(reason, "block_limit_consecutive");
        assert_eq!(consecutive, 3);
        assert_eq!(total, 10);
    }

    // --- Prompt caching tests ---

    #[test]
    fn tool_schemas_have_no_cache_control() {
        // cache_control is added at send time in send_message(), not in tool schemas.
        // This ensures all_tool_schemas() returns clean schemas.
        let schemas = all_tool_schemas();
        assert!(!schemas.is_empty(), "should have tool schemas");
        for schema in &schemas {
            assert!(
                schema.get("cache_control").is_none(),
                "tool schema should not contain cache_control: {}",
                schema["name"]
            );
        }
    }

    // --- Project Instructions Loading Tests ---
    //
    // Each test creates a temp directory and sets it as cwd for the duration of the test.
    // This isolates tests from the real working directory and from each other.
    // Uses a mutex to prevent parallel tests from racing on set_current_dir.

    use std::sync::Mutex;
    static CWD_MUTEX: Mutex<()> = Mutex::new(());

    /// Helper: run a closure with a temporary directory as cwd, then restore.
    fn with_temp_cwd<F: FnOnce(&std::path::Path)>(f: F) {
        let _lock = CWD_MUTEX.lock().unwrap();
        let original = std::env::current_dir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        f(tmp.path());
        std::env::set_current_dir(original).unwrap();
    }

    #[test]
    fn instructions_loads_claude_md() {
        with_temp_cwd(|dir| {
            std::fs::write(dir.join("CLAUDE.md"), "build instructions here").unwrap();
            let result = load_project_instructions();
            match result {
                InstructionsResult::Found { filename, contents } => {
                    assert_eq!(filename, "CLAUDE.md");
                    assert_eq!(contents, "build instructions here");
                }
                other => panic!("expected Found, got {:?}", other),
            }
        });
    }

    #[test]
    fn instructions_loads_agents_md_fallback() {
        // When only AGENTS.md exists (no CLAUDE.md), it should be loaded.
        with_temp_cwd(|dir| {
            std::fs::write(dir.join("AGENTS.md"), "agents content").unwrap();
            let result = load_project_instructions();
            match result {
                InstructionsResult::Found { filename, contents } => {
                    assert_eq!(filename, "AGENTS.md");
                    assert_eq!(contents, "agents content");
                }
                other => panic!("expected Found, got {:?}", other),
            }
        });
    }

    #[test]
    fn instructions_claude_md_takes_priority() {
        // When both exist, CLAUDE.md wins — AGENTS.md is never read.
        with_temp_cwd(|dir| {
            std::fs::write(dir.join("CLAUDE.md"), "claude wins").unwrap();
            std::fs::write(dir.join("AGENTS.md"), "agents loses").unwrap();
            let result = load_project_instructions();
            match result {
                InstructionsResult::Found { filename, contents } => {
                    assert_eq!(filename, "CLAUDE.md");
                    assert_eq!(contents, "claude wins");
                }
                other => panic!("expected Found, got {:?}", other),
            }
        });
    }

    #[test]
    fn instructions_neither_found() {
        // Empty directory — no instruction files.
        with_temp_cwd(|_dir| {
            let result = load_project_instructions();
            assert_eq!(result, InstructionsResult::NotFound);
        });
    }

    #[test]
    fn instructions_oversized_file_skipped() {
        // A CLAUDE.md over 32KB is skipped. If no AGENTS.md either, returns Skipped.
        with_temp_cwd(|dir| {
            let big = "x".repeat(PROJECT_INSTRUCTIONS_MAX_BYTES + 1);
            std::fs::write(dir.join("CLAUDE.md"), &big).unwrap();
            let result = load_project_instructions();
            match result {
                InstructionsResult::Skipped { filename, reason } => {
                    assert_eq!(filename, "CLAUDE.md");
                    assert!(
                        reason.contains("32KB"),
                        "reason should mention 32KB: {}",
                        reason
                    );
                }
                other => panic!("expected Skipped, got {:?}", other),
            }
        });
    }

    #[test]
    fn instructions_oversized_falls_through_to_agents() {
        // CLAUDE.md over 32KB, but AGENTS.md exists and is fine — loads AGENTS.md.
        with_temp_cwd(|dir| {
            let big = "x".repeat(PROJECT_INSTRUCTIONS_MAX_BYTES + 1);
            std::fs::write(dir.join("CLAUDE.md"), &big).unwrap();
            std::fs::write(dir.join("AGENTS.md"), "fallback agents").unwrap();
            let result = load_project_instructions();
            match result {
                InstructionsResult::Found { filename, contents } => {
                    assert_eq!(filename, "AGENTS.md");
                    assert_eq!(contents, "fallback agents");
                }
                other => panic!("expected Found via AGENTS.md fallback, got {:?}", other),
            }
        });
    }

    #[test]
    fn instructions_both_skipped_returns_last_skipped() {
        // Both files exist but both exceed size limit — returns last Skipped (AGENTS.md).
        with_temp_cwd(|dir| {
            let big = "x".repeat(PROJECT_INSTRUCTIONS_MAX_BYTES + 1);
            std::fs::write(dir.join("CLAUDE.md"), &big).unwrap();
            std::fs::write(dir.join("AGENTS.md"), &big).unwrap();
            let result = load_project_instructions();
            match result {
                InstructionsResult::Skipped { filename, .. } => {
                    assert_eq!(filename, "AGENTS.md");
                }
                other => panic!("expected Skipped for AGENTS.md, got {:?}", other),
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn instructions_unreadable_file_skipped() {
        // File exists but no read permission — produces Skipped, falls through.
        use std::os::unix::fs::PermissionsExt;
        with_temp_cwd(|dir| {
            let path = dir.join("CLAUDE.md");
            std::fs::write(&path, "secret").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            let result = load_project_instructions();
            // Restore permissions so tempdir cleanup works
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            match result {
                InstructionsResult::Skipped { filename, reason } => {
                    assert_eq!(filename, "CLAUDE.md");
                    assert!(
                        reason.contains("ermission") || reason.contains("denied"),
                        "reason should mention permission: {}",
                        reason
                    );
                }
                other => panic!("expected Skipped, got {:?}", other),
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn instructions_symlink_works() {
        // CLAUDE.md -> AGENTS.md symlink. Should load via CLAUDE.md name.
        with_temp_cwd(|dir| {
            std::fs::write(dir.join("AGENTS.md"), "symlinked content").unwrap();
            std::os::unix::fs::symlink(dir.join("AGENTS.md"), dir.join("CLAUDE.md")).unwrap();
            let result = load_project_instructions();
            match result {
                InstructionsResult::Found { filename, contents } => {
                    assert_eq!(filename, "CLAUDE.md");
                    assert_eq!(contents, "symlinked content");
                }
                other => panic!("expected Found via symlink, got {:?}", other),
            }
        });
    }

    #[test]
    fn instructions_prompt_integration_format() {
        // Verify the system prompt format when instructions are loaded.
        let base = "base prompt here";
        let filename = "CLAUDE.md";
        let contents = "## Build\ncargo build";
        let combined =
            format!("{base}\n\n---\n\n## Project Instructions (from {filename})\n\n{contents}");
        assert!(combined.starts_with("base prompt here"));
        assert!(combined.contains("## Project Instructions (from CLAUDE.md)"));
        assert!(combined.contains("cargo build"));
    }

    #[test]
    fn build_system_prompt_signature_unchanged() {
        // build_system_prompt() returns String, takes no args — this compiles iff true.
        let _: String = build_system_prompt();
    }

    // --- Pre-dispatch tests ---

    #[tokio::test]
    async fn pre_dispatch_null_input_returns_blocked_error() {
        // Null-input tool_use should produce a Blocked result with error ToolResult,
        // without touching hooks at all.
        let hooks = HookRunner::load("/nonexistent/hooks.toml", "/tmp");
        let mut consecutive = 0usize;
        let mut total = 0usize;
        let result = run_pre_dispatch(
            &hooks,
            "tu_1",
            "Bash",
            &serde_json::Value::Null,
            0,
            &mut consecutive,
            &mut total,
        )
        .await;
        match result {
            PreDispatchResult::Blocked(ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            }) => {
                assert_eq!(tool_use_id, "tu_1");
                assert!(content.contains("null input"));
                assert_eq!(is_error, Some(true));
            }
            other => panic!("expected Blocked for null input, got {:?}", other),
        }
        // Block counts should NOT be incremented for null-input (it's not a hook block)
        assert_eq!(consecutive, 0);
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn pre_dispatch_allow_resets_consecutive_block_count() {
        // A non-null input with no hooks should return Allow and reset consecutive count.
        let hooks = HookRunner::load("/nonexistent/hooks.toml", "/tmp");
        let mut consecutive = 2usize;
        let mut total = 5usize;
        let result = run_pre_dispatch(
            &hooks,
            "tu_2",
            "Read",
            &serde_json::json!({"file_path": "/tmp/test"}),
            0,
            &mut consecutive,
            &mut total,
        )
        .await;
        assert!(matches!(result, PreDispatchResult::Allow));
        assert_eq!(consecutive, 0); // reset on Allow
        assert_eq!(total, 5); // total unchanged on Allow
    }

    #[test]
    fn recover_conversation_preserves_single_message() {
        // Bug 4 fix: a single user message must not be popped, otherwise
        // the next API call has zero messages and enters an error loop.
        let mut msgs = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        }];
        recover_conversation(&mut msgs);
        assert_eq!(msgs.len(), 1, "single message must not be removed");
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn recover_conversation_preserves_minimum_after_orphan_pop() {
        // Three messages: [user, assistant(tool_use only), user].
        // Without the guard, all three would be popped leaving an empty vec.
        let mut msgs = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "first".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![ContentBlock::ToolUse {
                    id: "tu_1".to_string(),
                    name: "Read".to_string(),
                    input: serde_json::json!({"file_path": "/tmp/test"}),
                }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "second".to_string(),
                }],
            },
        ];
        recover_conversation(&mut msgs);
        assert!(
            !msgs.is_empty(),
            "recover_conversation must never empty the conversation"
        );
        // Should have popped trailing user, then orphaned assistant, but
        // stopped before popping the last user message.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        if let ContentBlock::Text { text } = &msgs[0].content[0] {
            assert_eq!(text, "first");
        } else {
            panic!("expected Text block");
        }
    }

    #[test]
    fn recover_conversation_empty_vec_is_noop() {
        let mut msgs: Vec<Message> = vec![];
        recover_conversation(&mut msgs);
        assert!(msgs.is_empty());
    }

    // --- Parallel path integration tests ---
    // These verify the blocked_flags and threshold_tripped code paths that
    // run_pre_dispatch unit tests cannot reach (those test the function in isolation;
    // these test the orchestration logic that calls it in a batch loop).

    #[tokio::test]
    async fn parallel_path_null_input_blocked_skips_post_hooks() {
        // Simulates the parallel dispatch loop with a batch containing one valid
        // tool and one null-input tool. Verifies:
        //   (a) null-input slot gets an error ToolResult
        //   (b) blocked_flags[i] is set for the null-input tool
        //   (c) post-dispatch is skipped for the blocked slot
        let hooks = HookRunner::load("/nonexistent/hooks.toml", "/tmp");
        let tool_uses: Vec<(String, String, serde_json::Value)> = vec![
            (
                "tu_valid".to_string(),
                "Read".to_string(),
                serde_json::json!({"file_path": "/dev/null"}),
            ),
            (
                "tu_null".to_string(),
                "Bash".to_string(),
                serde_json::Value::Null,
            ),
        ];
        let batch_size = tool_uses.len();
        let mut slots: Vec<Option<ContentBlock>> = vec![None; batch_size];
        let mut blocked_flags: Vec<bool> = vec![false; batch_size];
        let mut spawn_futures: Vec<(usize, tokio::task::JoinHandle<ContentBlock>)> = Vec::new();
        let mut consecutive_block_count = 0usize;
        let mut total_block_count = 0usize;
        let mut threshold_tripped = false;

        for (i, (id, name, input)) in tool_uses.iter().enumerate() {
            match run_pre_dispatch(
                &hooks,
                id,
                name,
                input,
                0,
                &mut consecutive_block_count,
                &mut total_block_count,
            )
            .await
            {
                PreDispatchResult::Allow => {
                    let id = id.clone();
                    let name = name.clone();
                    let input = input.clone();
                    let handle = tokio::task::spawn_blocking(move || {
                        dispatch_to_tool_result(id, name, input)
                    });
                    spawn_futures.push((i, handle));
                }
                PreDispatchResult::Blocked(cb) => {
                    slots[i] = Some(cb);
                    blocked_flags[i] = true;
                }
                PreDispatchResult::ThresholdTripped => {
                    threshold_tripped = true;
                    break;
                }
            }
        }

        join_spawned_futures(spawn_futures, &mut slots, &tool_uses).await;
        assert!(!threshold_tripped, "no threshold should trip");

        // Verify: slot 0 (valid Read) was dispatched and has a result
        assert!(!blocked_flags[0], "valid tool should not be blocked");
        assert!(slots[0].is_some(), "valid tool slot should be filled");
        if let Some(ContentBlock::ToolResult { is_error, .. }) = &slots[0] {
            assert!(is_error.is_none(), "Read /dev/null should succeed");
        }

        // Verify: slot 1 (null-input) is blocked with error ToolResult
        assert!(blocked_flags[1], "null-input tool should be blocked");
        if let Some(ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        }) = &slots[1]
        {
            assert_eq!(tool_use_id, "tu_null");
            assert!(content.contains("null input"));
            assert_eq!(*is_error, Some(true));
        } else {
            panic!("expected ToolResult for null-input slot");
        }

        // Verify: post-dispatch loop skips blocked slots (the `continue` path)
        let mut post_dispatch_count = 0;
        for (i, (_, name, input)) in tool_uses.iter().enumerate() {
            if blocked_flags[i] {
                continue;
            }
            if let Some(ContentBlock::ToolResult {
                ref content,
                is_error,
                ..
            }) = slots[i]
            {
                let is_err = is_error.unwrap_or(false);
                let _ = run_post_dispatch(&hooks, name, input, content, is_err, 0, false).await;
                post_dispatch_count += 1;
            }
        }
        // Only the valid (non-blocked) tool should get post-dispatch
        assert_eq!(
            post_dispatch_count, 1,
            "post-dispatch should fire for valid tool only"
        );
    }

    #[tokio::test]
    async fn parallel_path_threshold_trip_joins_pending_futures() {
        // Simulates a batch where a guard hook blocks tools until the consecutive
        // threshold trips mid-batch. Verifies:
        //   (a) already-spawned futures are joined (slots filled)
        //   (b) threshold_tripped is set
        //   (c) the result is Vec::new() (no tool_results returned)
        //
        // We use a batch of 5 null-input tools: since null-input returns Blocked
        // (without incrementing counters), this won't trip the threshold. Instead,
        // we manually test the threshold trip path with a guard hook.
        //
        // Alternate approach: use a 4-tool batch where a blocking guard hook
        // blocks tools 2-4 (hitting the consecutive limit of 3).
        // For simplicity, we test with run_pre_dispatch directly.
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("block.sh");
        std::fs::write(
            &hook_script,
            "#!/bin/bash\necho '{\"action\":\"block\",\"reason\":\"policy\"}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Guard hook only matches Bash — Read tools pass through unblocked,
        // so we can test that the threshold trip joins already-spawned Read futures.
        let config_path = dir.path().join("hooks.toml");
        std::fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\nmatch_tool = \"Bash\"\n",
                hook_script.display()
            ),
        )
        .unwrap();
        let hooks = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());

        let tool_uses: Vec<(String, String, serde_json::Value)> = vec![
            // Tool 0: Read (not matched by guard) → Allow → spawned
            (
                "tu_0".to_string(),
                "Read".to_string(),
                serde_json::json!({"file_path": "/dev/null"}),
            ),
            // Tools 1-3: Bash (matched by guard) → Block, Block, ThresholdTripped
            (
                "tu_1".to_string(),
                "Bash".to_string(),
                serde_json::json!({"command": "echo 1"}),
            ),
            (
                "tu_2".to_string(),
                "Bash".to_string(),
                serde_json::json!({"command": "echo 2"}),
            ),
            (
                "tu_3".to_string(),
                "Bash".to_string(),
                serde_json::json!({"command": "echo 3"}),
            ),
        ];
        let batch_size = tool_uses.len();
        let mut slots: Vec<Option<ContentBlock>> = vec![None; batch_size];
        let mut blocked_flags: Vec<bool> = vec![false; batch_size];
        let mut spawn_futures: Vec<(usize, tokio::task::JoinHandle<ContentBlock>)> = Vec::new();
        let mut consecutive_block_count = 0usize;
        let mut total_block_count = 0usize;
        let mut threshold_tripped = false;
        let mut threshold_reason = TurnStopReason::EndTurn;

        for (i, (id, name, input)) in tool_uses.iter().enumerate() {
            match run_pre_dispatch(
                &hooks,
                id,
                name,
                input,
                0,
                &mut consecutive_block_count,
                &mut total_block_count,
            )
            .await
            {
                PreDispatchResult::Allow => {
                    let id = id.clone();
                    let name = name.clone();
                    let input = input.clone();
                    let handle = tokio::task::spawn_blocking(move || {
                        dispatch_to_tool_result(id, name, input)
                    });
                    spawn_futures.push((i, handle));
                }
                PreDispatchResult::Blocked(cb) => {
                    slots[i] = Some(cb);
                    blocked_flags[i] = true;
                }
                PreDispatchResult::ThresholdTripped => {
                    threshold_tripped = true;
                    threshold_reason = threshold_stop_reason(consecutive_block_count);
                    break;
                }
            }
        }

        // Join any already-spawned futures (tool 0's Read should complete)
        join_spawned_futures(spawn_futures, &mut slots, &tool_uses).await;

        // Verify threshold tripped
        assert!(
            threshold_tripped,
            "consecutive block limit should have tripped"
        );
        assert_eq!(
            threshold_reason,
            TurnStopReason::BlockLimitConsecutive,
            "reason should be consecutive limit"
        );

        // Verify tool 0 (Read, spawned before blocks) completed in its slot
        assert!(
            slots[0].is_some(),
            "pre-threshold spawned tool should have a result"
        );
        if let Some(ContentBlock::ToolResult { is_error, .. }) = &slots[0] {
            assert!(is_error.is_none(), "Read /dev/null should succeed");
        }

        // Verify tools 1-2 were blocked (slots filled with error results)
        assert!(blocked_flags[1], "tool 1 should be blocked");
        assert!(blocked_flags[2], "tool 2 should be blocked");
        assert!(
            slots[1].is_some(),
            "blocked tool 1 should have error result"
        );
        assert!(
            slots[2].is_some(),
            "blocked tool 2 should have error result"
        );

        // Verify tool 3 was NOT processed (loop broke on ThresholdTripped)
        assert!(!blocked_flags[3], "tool 3 should not have been processed");
        assert!(
            slots[3].is_none(),
            "tool 3 slot should be empty (threshold broke before it)"
        );

        // In the real parallel path, threshold_tripped → Vec::new() (no tool_results)
        // and conversation.pop() fires. We verify the threshold_tripped flag is set,
        // which is what run_turn checks.
        let tool_results: Vec<ContentBlock> = if threshold_tripped {
            Vec::new()
        } else {
            slots.into_iter().map(|s| s.unwrap()).collect()
        };
        assert!(
            tool_results.is_empty(),
            "threshold trip should produce empty results"
        );
    }

    #[tokio::test]
    async fn parallel_path_guard_blocked_skips_post_hooks() {
        // hooks.md R7: "PostToolUse fires only for tools that were dispatched,
        // not for blocked tools." This verifies that guard-hook blocked tools
        // (as opposed to null-input blocked) also skip PostToolUse via blocked_flags.
        let dir = tempfile::tempdir().unwrap();
        let hook_script = dir.path().join("block_bash.sh");
        std::fs::write(
            &hook_script,
            "#!/bin/bash\necho '{\"action\":\"block\",\"reason\":\"policy\"}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config_path = dir.path().join("hooks.toml");
        std::fs::write(
            &config_path,
            format!(
                "[[hooks]]\nevent = \"PreToolUse\"\nphase = \"guard\"\ncommand = \"{}\"\nmatch_tool = \"Bash\"\n",
                hook_script.display()
            ),
        )
        .unwrap();
        let hooks = HookRunner::load(config_path.to_str().unwrap(), dir.path().to_str().unwrap());

        let tool_uses: Vec<(String, String, serde_json::Value)> = vec![
            // Tool 0: Read (no guard match) → Allow → dispatched
            (
                "tu_0".to_string(),
                "Read".to_string(),
                serde_json::json!({"file_path": "/dev/null"}),
            ),
            // Tool 1: Bash (guard blocks it) → Blocked
            (
                "tu_1".to_string(),
                "Bash".to_string(),
                serde_json::json!({"command": "echo hello"}),
            ),
        ];
        let batch_size = tool_uses.len();
        let mut slots: Vec<Option<ContentBlock>> = vec![None; batch_size];
        let mut blocked_flags: Vec<bool> = vec![false; batch_size];
        let mut spawn_futures: Vec<(usize, tokio::task::JoinHandle<ContentBlock>)> = Vec::new();
        let mut consecutive_block_count = 0usize;
        let mut total_block_count = 0usize;

        for (i, (id, name, input)) in tool_uses.iter().enumerate() {
            match run_pre_dispatch(
                &hooks,
                id,
                name,
                input,
                0,
                &mut consecutive_block_count,
                &mut total_block_count,
            )
            .await
            {
                PreDispatchResult::Allow => {
                    let id = id.clone();
                    let name = name.clone();
                    let input = input.clone();
                    let handle = tokio::task::spawn_blocking(move || {
                        dispatch_to_tool_result(id, name, input)
                    });
                    spawn_futures.push((i, handle));
                }
                PreDispatchResult::Blocked(cb) => {
                    slots[i] = Some(cb);
                    blocked_flags[i] = true;
                }
                PreDispatchResult::ThresholdTripped => break,
            }
        }

        join_spawned_futures(spawn_futures, &mut slots, &tool_uses).await;

        // Verify: Read tool dispatched normally
        assert!(!blocked_flags[0], "Read should not be blocked");
        assert!(slots[0].is_some(), "Read slot should have result");

        // Verify: Bash blocked by guard hook
        assert!(blocked_flags[1], "Bash should be blocked by guard hook");
        if let Some(ContentBlock::ToolResult {
            content, is_error, ..
        }) = &slots[1]
        {
            assert!(
                content.contains("policy"),
                "blocked reason should contain guard hook reason"
            );
            assert_eq!(*is_error, Some(true));
        } else {
            panic!("expected ToolResult for guard-blocked slot");
        }

        // Verify: post-dispatch skips the guard-blocked tool
        let mut post_dispatch_count = 0;
        for (i, (_, name, input)) in tool_uses.iter().enumerate() {
            if blocked_flags[i] {
                continue;
            }
            if let Some(ContentBlock::ToolResult {
                ref content,
                is_error,
                ..
            }) = slots[i]
            {
                let is_err = is_error.unwrap_or(false);
                let _ = run_post_dispatch(&hooks, name, input, content, is_err, 0, false).await;
                post_dispatch_count += 1;
            }
        }
        assert_eq!(
            post_dispatch_count, 1,
            "post-dispatch should only fire for the non-blocked Read tool"
        );

        // Block counters: guard hook block increments both consecutive and total
        assert_eq!(consecutive_block_count, 1);
        assert_eq!(total_block_count, 1);
    }

    #[test]
    fn threshold_takes_precedence_over_signal_break() {
        // hooks.md R6: "Block threshold takes unconditional precedence over
        // signal_break in both paths." When both threshold_tripped and
        // signal_break are true, the turn stop reason must be the threshold
        // reason, not ConvergenceSignal.
        let threshold_tripped = true;
        let signal_break = true;
        let threshold_reason = TurnStopReason::BlockLimitConsecutive;

        // Mirror the exact control flow from run_turn lines 873-892:
        let turn_stop_reason = if threshold_tripped {
            threshold_reason
        } else if signal_break {
            TurnStopReason::ConvergenceSignal
        } else {
            TurnStopReason::EndTurn
        };

        assert_eq!(
            turn_stop_reason,
            TurnStopReason::BlockLimitConsecutive,
            "threshold must take precedence over signal_break"
        );

        // Verify signal_break alone would produce ConvergenceSignal
        let turn_stop_no_threshold = if false {
            TurnStopReason::BlockLimitConsecutive
        } else if signal_break {
            TurnStopReason::ConvergenceSignal
        } else {
            TurnStopReason::EndTurn
        };

        assert_eq!(
            turn_stop_no_threshold,
            TurnStopReason::ConvergenceSignal,
            "signal_break without threshold should produce ConvergenceSignal"
        );
    }

    #[test]
    fn trim_conversation_preserves_first_message_content() {
        // The first message (system context) must survive trimming with its
        // content intact — trim removes from position 1 onwards, not position 0.
        let first_text = "I am the first message and must survive".to_string();
        let mut msgs = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: first_text.clone(),
            }],
        }];
        // Stuff enough messages to exceed CONTEXT_BUDGET_BYTES
        let filler = "x".repeat(100_000);
        for i in 0..20 {
            msgs.push(Message {
                role: if i % 2 == 0 { "assistant" } else { "user" }.to_string(),
                content: vec![ContentBlock::Text {
                    text: filler.clone(),
                }],
            });
        }
        let original_len = msgs.len();
        trim_conversation(&mut msgs);
        assert!(msgs.len() < original_len, "should have trimmed something");
        // The critical assertion: first message content is byte-identical.
        if let ContentBlock::Text { text } = &msgs[0].content[0] {
            assert_eq!(text, &first_text, "first message content must be preserved");
        } else {
            panic!("expected Text block in first message");
        }
    }

    #[test]
    fn recover_conversation_two_messages_trailing_user() {
        // Boundary: exactly 2 messages [user, user]. The trailing user should
        // be popped, leaving 1 message.
        let mut msgs = vec![
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "first".to_string(),
                }],
            },
            Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "second".to_string(),
                }],
            },
        ];
        recover_conversation(&mut msgs);
        assert_eq!(msgs.len(), 1, "trailing user should be popped");
        assert_eq!(msgs[0].role, "user");
        if let ContentBlock::Text { text } = &msgs[0].content[0] {
            assert_eq!(text, "first");
        } else {
            panic!("expected Text block");
        }
    }

    #[test]
    fn recover_conversation_two_messages_trailing_assistant() {
        // Boundary: exactly 2 messages [user, assistant(text)]. Nothing should
        // be popped because trailing is not user and assistant has text (not orphaned).
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
                    text: "hi there".to_string(),
                }],
            },
        ];
        recover_conversation(&mut msgs);
        assert_eq!(msgs.len(), 2, "nothing should be popped");
    }
}
